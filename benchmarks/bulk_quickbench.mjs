/** Independent typed-array control. No application reducer or private fixture. */
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { performance } from 'node:perf_hooks';
import { cpus, platform, release } from 'node:os';

const argv = process.argv.slice(2);
function arg(name, fallback) { const i = argv.indexOf(name); return i < 0 ? fallback : argv[i + 1]; }
const output = resolve(arg('--output', 'scratch/bulk-benchmark.json'));
if (existsSync(output)) throw new Error('Use a fresh output filename');
const require = createRequire(join(process.cwd(), 'package.json'));
const entry = arg('--module') ?? require.resolve('@skarve/engine');
const api = await import(pathToFileURL(resolve(entry)).href);
const library = arg('--library') ?? join(dirname(entry), 'native/linux-x64/libraster_engine.so');
const hash = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const libraryHash = hash(library);
if (arg('--expected-library-sha256')) assert.equal(libraryHash, arg('--expected-library-sha256'));
const seed = Number(arg('--seed', randomBytes(4).readUInt32LE(0)));
assert.ok(Number.isSafeInteger(seed) && seed >= 0 && seed <= 0xffffffff);
const smoke = argv.includes('--smoke');
const rounds = smoke ? 1 : 5;
const result = { schema: 1, seed, library_sha256: libraryHash, module_sha256: hash(entry),
  harness_sha256: hash(fileURLToPath(import.meta.url)), node: process.version,
  host: { platform: platform(), release: release(), cpu: cpus()[0]?.model },
  scope: 'Independent synthetic typed arrays. Complete native call includes owned snapshot, validation, FFI, native reduction, result copy and sink; arrays and selections already exist for both controls. No source retrieval, polygon selection or application code. Warmup explicit; repeated calls are descriptive.',
  limits: { logical_bands: 40, windows: 2, maximum_cells_per_window: smoke ? 1000 : 120000,
    native_owned_bytes_cap: api.BULK_LIMITS.globalOwnedBytes, process_rss_observed_not_virtual_address_capped: true },
  records: [], errors: [], correctness_fields: 0 };

function fixture(Type, count, cells, policy) {
  const windows = [];
  for (let window = 0; window < 2; window++) {
    const bands = [];
    for (let id = 0; id < count; id++) {
      const backing = new Type(cells + 2); const values = backing.subarray(1, cells + 1);
      const validity = new Uint8Array(cells); validity.fill(1);
      for (let cell = 0; cell < cells; cell++) {
        values[cell] = ((cell * 17 + id * 13 + window * 29 + seed % 307) % 307) / 16 - 7;
        if (cell % 79 === 0) validity[cell] = 0;
        else if (cell % 97 === 0) values[cell] = -9999;
        else if (policy === 'hm_demographics_ordered_v1' && cell % 113 === 0) values[cell] = NaN;
      }
      bands.push({ id, values, validity, nodata: -9999 });
    }
    windows.push({ bands });
  }
  return windows;
}

function independent(windows, policy) {
  const out = windows[0].bands.map(({ id }) => ({ id, has_values: false, sum: 0, min: null, max: null,
    mean: null, valid_count: 0, excluded_mask: 0, excluded_nodata: 0, excluded_nonfinite: 0,
    excluded_negative: 0, correction: 0 }));
  for (const window of windows) for (let bi = 0; bi < window.bands.length; bi++) {
    const band = window.bands[bi], record = out[bi]; let partial = 0;
    for (let cell = 0; cell < band.values.length; cell++) {
      if (!band.validity[cell]) { record.excluded_mask++; continue; }
      const value = band.values[cell];
      if (value === band.nodata) { record.excluded_nodata++; continue; }
      if (!Number.isFinite(value)) {
        if (policy === 'strict_selected_v1') throw new Error('Nonfinite strict contribution');
        record.excluded_nonfinite++; continue;
      }
      if (policy === 'hm_demographics_ordered_v1' && value < 0) { record.excluded_negative++; continue; }
      if (policy === 'hm_demographics_ordered_v1') partial += value;
      else {
        const next = record.sum + value;
        record.correction += Math.abs(record.sum) >= Math.abs(value) ? (record.sum - next) + value : (value - next) + record.sum;
        record.sum = next;
      }
      record.min = record.min === null ? value : Math.min(record.min, value);
      record.max = record.max === null ? value : Math.max(record.max, value);
      record.valid_count++;
    }
    if (policy === 'hm_demographics_ordered_v1') record.sum += partial;
  }
  return out.map(record => {
    if (policy === 'strict_selected_v1') record.sum += record.correction;
    delete record.correction;
    if (!Number.isFinite(record.sum)) throw new Error('Nonfinite sum');
    record.has_values = record.valid_count > 0;
    record.mean = record.has_values ? record.sum / record.valid_count : null;
    return record;
  });
}

for (const Type of [Float32Array, Float64Array]) for (const bands of [36, 40])
for (const cells of (smoke ? [1000] : [1000, 120000]))
for (const policy of ['strict_selected_v1', 'hm_demographics_ordered_v1']) {
  const name = `${Type.name}-${bands}x${cells}-${policy}`;
  const windows = fixture(Type, bands, cells, policy);
  const engine = new api.RasterEngine({ library });
  try {
    const native = () => engine.bulkReduce(windows, { policy, reducers: ['sum', 'min', 'max', 'mean'] });
    // Neither warmup is scored. JIT and native initialization are explicit.
    for (let i = 0; i < 2; i++) { independent(windows, policy); await native(); }
    for (let round = 0; round < rounds; round++) {
      const order = (round + seed) % 2 ? ['native', 'javascript'] : ['javascript', 'native'];
      const answers = {};
      for (const system of order) {
        const began = performance.now();
        const bandsOut = system === 'native' ? (await native()).bands : independent(windows, policy);
        const sink = JSON.stringify(bandsOut); const elapsed = performance.now() - began;
        answers[system] = bandsOut;
        result.records.push({ case: name, dtype: Type.name, bands, cells, windows: 2, policy,
          round, system, elapsed_ms: elapsed, sink_bytes: Buffer.byteLength(sink),
          source_payload_bytes: bands * cells * 2 * (Type.BYTES_PER_ELEMENT + 1),
          timing: system === 'native' ? { ...engine.lastTiming } : null, answers: bandsOut });
      }
      assert.deepEqual(answers.native, answers.javascript);
      result.correctness_fields += answers.native.length * Object.keys(answers.native[0]).length;
    }
  } catch (error) { result.errors.push({ case: name, error: `${error.name}: ${error.message}` }); }
  finally { await engine.close(); assert.equal(api.bulkMemoryStats().owned_bytes, 0); }
}
result.max_rss_bytes = process.resourceUsage().maxRSS * 1024;
result.native_owned_bytes_after = api.bulkMemoryStats().owned_bytes;
result.pass = result.errors.length === 0;
mkdirSync(dirname(output), { recursive: true });
writeFileSync(output, JSON.stringify(result, null, 2) + '\n');
console.log(JSON.stringify({ output, records: result.records.length, errors: result.errors.length,
  correctness_fields: result.correctness_fields, max_rss_bytes: result.max_rss_bytes }));
assert.ok(result.pass);
