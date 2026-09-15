/** Installed source-owned conversion; no Python worker or source arrays. */
import Skarve from '@skarve/engine';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

const [fixturePath, output, predictor = 'none', payload_layout = 'band'] = process.argv.slice(2);
if (!fixturePath || !output || !['none', 'byte_delta_v1'].includes(predictor) || !['band', 'row_group_v1'].includes(payload_layout)) {
  throw new Error('Usage: node node_skv.mjs fixture.json new-output.skv [none|byte_delta_v1] [band|row_group_v1]');
}
const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
const engine = new Skarve();
try {
  const original = await engine.infuse(join(dirname(fixturePath), fixture.sources.original.file));
  const compilation = await original.compile(output, { chunk_edge: 64, band_group: 4, predictor, payload_layout });
  if (compilation.predictor !== predictor) throw new Error('Unexpected compiled predictor');
  if (compilation.payload_layout !== payload_layout) throw new Error('Unexpected compiled payload layout');
  await original.close();
  const verification = await engine.verifySkv(output);
  const source = await engine.infuse(output);
  const single = await source.carve({ zone: fixture.geometries.small, metrics: ['sum', 'support', 'mean', 'min', 'max'] });
  await source.close();
  let rows = 0, complete = false, batchMetrics;
  for await (const page of engine.cleave({
    zones: ['small', 'overlap'].map(id => ({ id, version: '1', geometry: fixture.geometries[id] })),
    slices: [{ id: 'snapshot', spec: { location: output } }], crs: fixture.crs,
    metrics: ['sum', 'support', 'mean', 'min', 'max'],
  }, { maxRows: 1 })) {
    rows += page.rows.length; complete = page.complete; batchMetrics = page.metrics;
  }
  if (!complete || rows !== 2) throw new Error('Incomplete batch');
  console.log(JSON.stringify({ compilation, verification, single, batch_rows: rows, batch_metrics: batchMetrics }));
} finally { await engine.close(); }
