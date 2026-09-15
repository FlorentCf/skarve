/** Consume a provisioned same-view profile using the installed native engine. */
import fs from 'node:fs';
import Skarve from '@skarve/engine';

const [profilePath, requestPath, viewId, accessClass = 'unknown'] = process.argv.slice(2);
if (!profilePath || !requestPath || !viewId)
  throw new Error('Usage: node node_serving.mjs PROFILE.json REQUEST.json VIEW_ID [ACCESS_CLASS]');
const profile = JSON.parse(fs.readFileSync(profilePath, 'utf8'));
const request = JSON.parse(fs.readFileSync(requestPath, 'utf8'));
const engine = new Skarve();
try {
  const result = await engine.sumSelected(profile, request, {
    numerical_policy: 'hm_demographics_ordered_v1', view_id: viewId, access_class: accessClass,
  });
  if (!result.complete) throw new Error('Incomplete ordered result');
  console.log(JSON.stringify(result));
} finally {
  await engine.close();
}
