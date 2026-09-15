/** Generated-only raw ordered selections, with an independent JavaScript oracle. */
import Skarve from '@skarve/engine';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const [fixturePath, scratch] = process.argv.slice(2);
if (!fixturePath || !scratch) throw new Error('Usage: node node_ordered.mjs fixture.json new-scratch-directory');
const policy = 'hm_demographics_ordered_v1';
const mapping = [2, 0, 1];
const request = { bands: [1, 0], polygons: [
  { id: 'rectangle', windows: [{ window: [2, 3, 9, 5], runs: [[0, 45]] }] },
  { id: 'shifted-overlap', windows: [
    { window: [0, 0, 8, 5], runs: [[0, 8], [10, 17], [24, 33]] },
    { window: [3, 2, 8, 5], indexes: [0, 2, 3, 8, 9, 15, 18, 24, 25, 31, 39] },
    { window: [1, 1, 6, 4], runs: [[2, 9], [12, 17], [20, 24]] },
  ] },
  { id: 'empty', windows: [{ window: [0, 0, 4, 4], runs: [] }] },
] };
const rectangle = { type: 'Polygon', coordinates: [[[2, 56], [11, 56], [11, 61], [2, 61], [2, 56]]] };
// Reconstruct the generated algebra without reading or normalizing raster samples.
function sample(band, x, y) {
  return Math.fround((x + 2 * y + band) % 31 === 0 ? -9999 : (x + 3 * y + 11 * band) % 61 - 20 + 2 * band);
}
function* indexes(window) {
  if (window.indexes) yield* window.indexes;
  else for (const [start, end] of window.runs) for (let i = start; i < end; i++) yield i;
}
const expected = request.polygons.map(polygon => ({ id: polygon.id, bands: request.bands.map(id => {
  const band = mapping[id];
  const result = { id, sum: 0, has_values: false, valid_count: 0, excluded_mask: 0,
    excluded_nodata: 0, excluded_nonfinite: 0, excluded_negative: 0 };
  for (const window of polygon.windows) {
    const [x, y, width] = window.window;
    let partial = 0;
    for (const index of indexes(window)) {
      const value = sample(band, x + index % width, y + Math.floor(index / width));
      if (!Number.isFinite(value)) result.excluded_nonfinite++;
      else if (value === -9999) result.excluded_nodata++;
      else if (value < 0) result.excluded_negative++;
      else { result.valid_count++; partial += value; }
    }
    // Logical window partials fold in request order, independent of physical reads.
    result.sum += partial;
  }
  result.has_values = result.valid_count > 0;
  return result;
}) }));
const fractionalExpected = request.bands.map(id => {
  const band = mapping[id], scales = [.5, 2, -1], offsets = [10, -4, 3];
  let sum = 0, count = 0;
  for (let y = 3; y < 8; y++) for (let x = 2; x < 11; x++) {
    const raw = sample(band, x, y);
    if ((x + y) % 17 !== 0 && raw !== -9999) { sum += raw * scales[band] + offsets[band]; count++; }
  }
  return { band: id, fractional_sum: sum, covered_cell_equivalents: count };
});

const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
assert.equal(fixture.origin, 'Deterministically generated mathematical test data; no external dataset.');
const original = join(dirname(fixturePath), fixture.sources.original.file);
const sha = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const originalSha = sha(original);
assert.equal(originalSha, fixture.sources.original.sha256);
assert(!existsSync(scratch), 'Use a new scratch directory');
mkdirSync(scratch, { recursive: true });
const owned = join(scratch, 'generated-original.tif'), compiled = join(scratch, 'ordered.skv');
copyFileSync(original, owned);
const engine = new Skarve();
let direct, serving;
try {
  const source = await engine.infuse({ location: owned, bands: mapping });
  direct = await source.sumSelected(request, { numerical_policy: policy });
  assert(direct.complete); assert.equal(direct.numerical_policy, policy); assert.deepEqual(direct.rows, expected);
  const fractional = await source.carve({ zone: rectangle, bands: request.bands, metrics: ['sum', 'support'] });
  for (let i = 0; i < fractionalExpected.length; i++) {
    for (const [key, value] of Object.entries(fractionalExpected[i])) assert.equal(fractional.bands[i][key], value);
  }
  assert.notDeepEqual(fractional.bands.map(b => b.fractional_sum), expected[0].bands.map(b => b.sum));
  await source.compile(compiled, { chunk_edge: 64, predictor: 'byte_delta_v1' });
  await source.close();
  // Rename only our generated copy; the shared input fixture remains intact.
  renameSync(owned, join(scratch, 'generated-original.unavailable'));
  assert(!existsSync(owned));
  const snapshot = await engine.infuse(compiled);
  serving = await snapshot.sumSelected(request, { numerical_policy: policy });
  assert.deepEqual(serving.rows, expected);
  assert.equal(serving.provenance.summaries_used, false);
  assert.equal(serving.provenance.source_layout.self_contained, true);
  await snapshot.close();
} finally { await engine.close(); }
assert.equal(sha(original), originalSha);
const selection = join(scratch, 'selections.json');
writeFileSync(selection, JSON.stringify(request, null, 2) + '\n');
console.log(JSON.stringify({ passed: true, policy, source_mapping: mapping, rows: expected,
  source_original_unavailable: !existsSync(owned), source: resolve(compiled), selection: resolve(selection),
  fractional_normalized_rectangle: fractionalExpected,
  comparison_contract: 'Different raw ordered and fractional normalized policies; not a compatibility or speed claim.',
  fixture_rounding_scope: 'These small Float32 integers sum exactly in binary64; rounding-sensitive window-order tests are separate.',
  direct_metrics: direct.metrics, skv_metrics: serving.metrics }));
