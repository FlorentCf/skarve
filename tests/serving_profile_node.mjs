/** Installed Node profile workflow, sparse fallback and cancellation recovery. */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import Skarve from '@skarve/engine';

const [directory] = process.argv.slice(2);
const profile = JSON.parse(fs.readFileSync(path.join(directory, 'profile.json'), 'utf8'));
const request = JSON.parse(fs.readFileSync(path.join(directory, 'request.json'), 'utf8'));
const options = { numerical_policy: profile.numerical_policy, view_id: profile.view_id };
assert.match(profile.direct.expected.raw_metadata.bands[0].scale_f64_bits, /^[a-f0-9]{16}$/);
const engine = new Skarve();
try {
  const accelerated = await engine.sumSelected(profile, request, { ...options, access_class: 'fixture-qualified-local' });
  const direct = await engine.sumSelected(profile, request, options);
  assert.equal(accelerated.routing.selected, 'accelerated');
  assert.equal(direct.routing.selected, 'direct');
  assert.deepEqual(accelerated.rows, direct.rows);
  assert(accelerated.routing.source_closed && !accelerated.routing.unselected_source_opened);
  const sparse = await engine.sumSelected(profile, { ...request, bands: [39] }, { ...options, access_class: 'fixture-qualified-local' });
  assert.equal(sparse.routing.selected, 'direct');
  assert.deepEqual(sparse.rows[0].bands, direct.rows[0].bands.slice(0, 1));
  await assert.rejects(engine.sumSelected(profile, request, { ...options, view_id: 'wrong-view' }));
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(engine.sumSelected(profile, request, { ...options, signal: controller.signal }));
  const recovered = await engine.sumSelected(profile, request, { ...options, access_class: 'fixture-qualified-local' });
  assert.deepEqual(recovered.rows, direct.rows);
  console.log(JSON.stringify({ passed: true, rows: direct.rows, selected: ['accelerated', 'direct'],
    sparse: 'direct', cancellation_recovery: true, source_closed: recovered.routing.source_closed }));
} finally {
  await engine.close();
}
