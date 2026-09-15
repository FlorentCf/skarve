/** Run after skv_installed.py using the installed Node package. */
import Skarve from '@skarve/engine';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';

const path = process.argv[2];
if (!path) throw new Error('Usage: node skv_installed_node.mjs fixture.json');
const fixture = JSON.parse(readFileSync(path, 'utf8'));
const metrics = ['sum', 'support', 'mean', 'min', 'max'];
function equal(actual, expected) {
  assert.equal(actual.length, expected.length);
  for (let band = 0; band < actual.length; band++) {
    assert.deepEqual(Object.keys(actual[band]).sort(), Object.keys(expected[band]).sort());
    for (const key of Object.keys(expected[band])) {
      const got = actual[band][key], want = expected[band][key];
      if (typeof got === 'number') {
        const absolute = want !== 0 && Math.abs(want) < 1e-6 ? 0 : 1e-10;
        assert(Math.abs(got - want) <= Math.max(absolute, 2e-13 * Math.max(Math.abs(got), Math.abs(want))), `${band}:${key}`);
      } else assert.deepEqual(got, want);
    }
  }
}
const engine = new Skarve();
try {
  assert(!existsSync(fixture.original_unavailable));
  const output = join(dirname(path), 'node.skv');
  const original = await engine.infuse(fixture.original_for_compile);
  const aborted = new AbortController(); aborted.abort();
  await assert.rejects(original.compile(output, { signal: aborted.signal }), { code: 'ABORTED' });
  assert(!existsSync(output));
  const compilation = await original.compile(output, { chunk_edge: 64, band_group: 4 });
  assert.equal(compilation.predictor, 'none');
  assert.equal(compilation.payload_layout, 'band');
  await assert.rejects(original.compile(output), /exist|overwrite/);
  const plainOutput = join(dirname(path), 'node-none.skv');
  await original.compile(plainOutput, { chunk_edge: 64, band_group: 4, codec: 'none' });
  const predictedOutputs = [], predictorReceipts = [];
  const groupedOutputs = [], groupReceipts = [];
  for (const codec of ['none', 'deflate']) {
    const predicted = join(dirname(path), `node-byte-delta-${codec}.skv`);
    const built = await original.compile(predicted, { chunk_edge: 64, band_group: 4, codec, predictor: 'byte_delta_v1' });
    assert.equal(built.predictor, 'byte_delta_v1');
    assert.equal(built.logical_digest, compilation.logical_digest);
    predictedOutputs.push(predicted);
    predictorReceipts.push({ codec, predictor: built.predictor, byte_length: built.byte_length, logical_digest: built.logical_digest });
  }
  for (const codec of ['none', 'deflate']) {
    const grouped = join(dirname(path), `node-grouped-${codec}.skv`);
    const built = await original.compile(grouped, { chunk_edge: 64, band_group: 40, codec, predictor: 'byte_delta_v1', payload_layout: 'row_group_v1' });
    assert.equal(built.payload_layout, 'row_group_v1');
    assert.equal(built.logical_digest, compilation.logical_digest);
    groupedOutputs.push(grouped);
    groupReceipts.push({ codec, payload_layout: built.payload_layout, byte_length: built.byte_length, logical_digest: built.logical_digest });
  }
  await original.close();
  await engine.verifySkv(output);
  await engine.verifySkv(plainOutput);
  for (const predicted of [...predictedOutputs, ...groupedOutputs]) await engine.verifySkv(predicted);
  for (const objectPath of [output, plainOutput, ...predictedOutputs, ...groupedOutputs]) {
    for (const use_summaries of [true, false]) {
      const source = await engine.infuse({ location: objectPath, use_summaries });
      assert.deepEqual((await source.sumSelected(fixture.ordered_request, { numerical_policy: 'hm_demographics_ordered_v1' })).rows, fixture.ordered_reference);
      for (const [name, zone] of Object.entries(fixture.zones)) {
        equal((await source.carve({ zone, metrics })).bands, fixture.reference[name]);
      }
      equal((await source.carve({ zone: fixture.zones.whole, metrics, bands: [39] })).bands, [fixture.reference.whole[39]]);
      await source.close();
      for (const selection of [{}, { bands: [39] }]) {
        let rows = 0, complete = false;
        for await (const page of engine.cleave({ zones: Object.entries(fixture.zones).map(([id, geometry]) => ({ id, version: '1', geometry })),
          slices: [{ id: 'snapshot', spec: { location: objectPath, use_summaries }, ...selection }], crs: 'EPSG:3857', metrics }, { maxRows: 2 })) {
          for (const row of page.rows) {
            const reference = fixture.reference[row.zone_id];
            equal(row.bands, selection.bands ? [reference[39]] : reference); rows++;
          }
          complete = page.complete;
        }
        assert(complete); assert.equal(rows, Object.keys(fixture.zones).length);
      }
    }
  }
  if (fixture.exactextract) {
    for (const objectPath of [output, ...predictedOutputs, ...groupedOutputs]) {
      const source = await engine.infuse(objectPath);
      const result = await source.carve({ zone: fixture.zones.whole, metrics, backend: 'exactextract' });
      assert.equal(result.provenance.selected_backend, 'exactextract');
      const fields = ['band', 'fractional_sum', 'covered_cell_equivalents', 'coverage_weighted_mean', 'min', 'max'];
      const select = bands => bands.map(band => Object.fromEntries(fields.map(key => [key, band[key]])));
      equal(select(result.bands), select(fixture.reference.whole));
      for (const selection of [{}, { bands: [39] }]) {
        for (const [name, zone] of Object.entries(fixture.zones)) {
          const expected = fixture.exactextract_reference[name];
          equal((await source.carve({ zone, metrics, backend: 'exactextract', ...selection })).bands,
            selection.bands ? [expected[39]] : expected);
        }
      }
      await source.close();
      for (const strategy of ['feature-sequential', 'raster-sequential']) {
        for (const selection of [{}, { bands: [39] }]) {
          let rows = 0, complete = false;
          for await (const page of engine.cleave({
            zones: Object.entries(fixture.zones).map(([id, geometry]) => ({ id, version: '1', geometry })),
            slices: [{ id: 'snapshot', spec: { location: objectPath }, ...selection }], crs: 'EPSG:3857', metrics,
            backend: 'exactextract', backend_options: { strategy },
          }, { maxRows: 2 })) {
            assert.equal(page.provenance.selected_backend, 'exactextract');
            for (const row of page.rows) {
              const expected = fixture.exactextract_reference[row.zone_id];
              equal(row.bands, selection.bands ? [expected[39]] : expected); rows++;
            }
            complete = page.complete;
          }
          assert(complete); assert.equal(rows, Object.keys(fixture.zones).length);
        }
      }
    }
  }
  console.log(JSON.stringify({ passed: true, bands: 40, cases: ['1x1', 'Nx1', '1xM', 'NxM'], compilation, predictorReceipts, groupReceipts, exactextract: fixture.exactextract }));
} finally { await engine.close(); }
