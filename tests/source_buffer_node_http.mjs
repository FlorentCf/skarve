/** Controlled HTTP correctness/request-count test; no latency benchmark or cloud access.
 * Pass a generated >=40-band TIFF/SKV fixture with at least a9x9 source grid.
 */
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
const { Skarve } = await import(process.env.SKARVE_TEST_MODULE ?? '../bindings/node/index.mjs');

const path = process.argv[2];
assert(path, 'Pass a generated40-band fixture');
const payload = readFileSync(path);
const observations = [];
let generation = '"fixture-one"', onGet = null;
const server = createServer((req, res) => {
  if (req.method === 'HEAD') {
    res.writeHead(200, {'Content-Length':payload.length, ETag:generation, 'Accept-Ranges':'bytes'}).end();
    return;
  }
  assert.equal(req.method,'GET');
  const match = /^bytes=(\d+)-(\d+)$/.exec(req.headers.range ?? '');
  assert(match);
  const start=Number(match[1]), end=Math.min(Number(match[2]),payload.length-1);
  observations.push({start,end,etag:req.headers['if-match']});
  if (req.headers['if-match'] && req.headers['if-match'] !== generation) {
    res.writeHead(412,{'Content-Length':0}).end(); return;
  }
  const etag=generation;
  onGet?.();
  res.writeHead(206,{'Content-Length':end-start+1,'Content-Range':`bytes ${start}-${end}/${payload.length}`, ETag:etag,'Accept-Ranges':'bytes'});
  res.end(payload.subarray(start,end+1));
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const spec={location:`http://127.0.0.1:${server.address().port}/fixture.${path.endsWith('.skv')?'skv':'tif'}?signed-test=1`,
  http:{allow_http:true,cache_bytes:0,max_requests:256,max_download_bytes:8<<20,max_range_bytes:4<<20,timeout_seconds:2}};
const engine=new Skarve();
const bytes=value=>Buffer.from(value.buffer,value.byteOffset,value.byteLength);
let groupedVerifications, controlVerifications;
try {
  const source=await engine.infuse(spec);
  assert(source.metadata.rawMetadata.bands.length>=40);
  assert.equal(source.metadata.rawMetadata.maxWindowBands,64);
  const selected=Array.from({length:40},(_,i)=>39-i);
  const first=observations.length;
  const complete=await source.readWindow({window:[1,1,7,8],bands:selected});
  const grouped=observations.slice(first);
  groupedVerifications=grouped.filter(r=>r.start===0&&r.end===0).length;
  assert.equal(groupedVerifications,2,'one pre/post pair for all40 bands');
  assert(grouped.every(r=>r.etag==='"fixture-one"'));
  assert(complete.rawReadGroups>=1);
  const controlStart=observations.length;
  for(let start=0;start<selected.length;start+=3) {
    const control=await source.readWindow({window:[1,1,7,8],bands:selected.slice(start,start+3)});
    for(const [i,band] of control.bands.entries()) {
      assert.deepEqual(bytes(complete.bands[start+i].values),bytes(band.values));
      assert.deepEqual(complete.bands[start+i].mask,band.mask);
      assert.deepEqual(complete.bands[start+i].metadata,band.metadata);
    }
  }
  controlVerifications=observations.slice(controlStart).filter(r=>r.start===0&&r.end===0).length;
  assert.equal(controlVerifications,28,'three-band caller control performs14 complete operations');
  const beforeMetrics = observations.length;
  const before = await source.metrics();
  assert.equal(before.verification, undefined, 'Metrics cannot provide a generation proof');
  assert.equal(observations.length, beforeMetrics, 'Observational metrics must not contact the source');
  const renewed = await source.beginQuery();
  assert.equal(renewed.verification, undefined, 'Budget renewal cannot provide a generation proof');
  assert.equal(observations.length, beforeMetrics, 'Renewal is not a source-identity assertion');
  assert.equal(renewed.diagnostics.remote.requests, before.diagnostics.remote.requests);
  assert.equal(renewed.diagnostics.remote.budget_requests_base, before.diagnostics.remote.requests);
  assert.equal(renewed.diagnostics.remote.budget_epoch, 1);
  const again = await source.readWindow({window:[1,1,7,8],bands:selected});
  for (const [i,band] of again.bands.entries()) {
    assert.deepEqual(bytes(band.values),bytes(complete.bands[i].values));
    assert.deepEqual(band.mask,complete.bands[i].mask);
  }
  assert.equal(observations.slice(beforeMetrics).filter(r=>r.start===0&&r.end===0).length, 2);
  await source.close();
  if(!path.endsWith('.skv')) {
    const larger=await engine.infuse({...spec,http:{...spec.http,cache_bytes:16<<20}});
    assert.equal(larger.metadata.diagnostics.adapter_retained_capacity_bytes,source.metadata.diagnostics.adapter_retained_capacity_bytes+(8<<20));
    const result=await larger.readWindow({window:[1,1,7,8],bands:selected});
    for(const [i,band] of result.bands.entries()) assert.deepEqual(bytes(band.values),bytes(complete.bands[i].values));
    await larger.close();
    await assert.rejects(engine.infuse({...spec,http:{...spec.http,cache_bytes:(16<<20)+1}}));
  } else await assert.rejects(engine.infuse({...spec,http:{...spec.http,cache_bytes:16<<20}}));

  // Several complete native reads share one generation bracket only when the
  // caller explicitly withholds the useful result until the scope succeeds.
  const scoped=await engine.infuse({...spec,http:{...spec.http,cache_bytes:1<<20}});
  const registered = scoped.metadata.registered_http_identity;
  assert.match(registered.source_id, /^http-etag-sha256:[a-f0-9]{64}$/);
  assert.equal(registered.etag, generation);
  assert.equal(registered.byte_length, payload.length);
  const beginProof = await scoped._beginVerifiedQuery({renewBudget:false});
  assert.equal(beginProof.verification, null);
  assert.equal(beginProof.verified_query_complete, false);
  await scoped.readWindow({window:[1,1,7,8],bands:selected});
  const finalProof = await scoped._endVerifiedQuery();
  assert.equal(finalProof.verified_query_complete, true);
  assert.deepEqual(finalProof.verification, {schema:'skarve_http_query_verification_v1',source:scoped.id,identity:registered});
  const scopeStart=observations.length;
  const scopedResult=await scoped.withVerifiedQuery(async s=>{
    const first=await s.readWindow({window:[1,1,7,8],bands:selected});
    const second=await s.readWindow({window:[1,1,7,8],bands:selected});
    for(const [i,band] of second.bands.entries()) assert.deepEqual(bytes(band.values),bytes(first.bands[i].values));
    return second;
  },{renewBudget:false});
  assert.equal(observations.slice(scopeStart).filter(r=>r.start===0&&r.end===0).length,1);
  for(const [i,band] of scopedResult.bands.entries()) assert.deepEqual(bytes(band.values),bytes(complete.bands[i].values));
  // No uncached payload needs to be fetched for this mutation to be caught.
  await assert.rejects(scoped.withVerifiedQuery(async s=>{
    await s.readWindow({window:[1,1,7,8],bands:selected});
    generation='"fixture-two"';
    return 'must not escape';
  }));
  assert.throws(()=>scoped.metrics(),/closed/);
  generation='"fixture-one"';
  const stale=await engine.infuse({...spec,http:{...spec.http,cache_bytes:1<<20}});
  await stale.readWindow({window:[1,1,7,8],bands:selected});
  generation='"fixture-two"';
  await assert.rejects(stale.withVerifiedQuery(async s=>{
    await s.readWindow({window:[1,1,7,8],bands:selected});
    return 'already-stale cached values must not escape';
  }));
  generation='"fixture-one"';

  // Invalid protocol usage may poison the reader (SKV deliberately does).
  // Test replay rejection on its own handle, never reuse a failed handle.
  const ended=await engine.infuse(spec);
  await ended._beginVerifiedQuery({renewBudget:false});
  await ended._endVerifiedQuery();
  await assert.rejects(ended._endVerifiedQuery(), /query|scope/i, 'A second end cannot reissue an old proof');
  await ended.close();

  // Change generation after the first conditional response. Cached or uncached
  // subsequent groups can never turn the mixed operation into a usable result.
  const changed=await engine.infuse(spec);
  onGet=()=>{generation='"fixture-two"';onGet=null;};
  await assert.rejects(changed.readWindow({window:[1,1,7,8],bands:selected}));
  await assert.rejects(changed.beginQuery(), 'A fresh budget cannot repair a failed generation');
  await changed.close();
  generation='"fixture-one"';
  const cancelled=await engine.infuse(spec);
  const controller=new AbortController();
  onGet=()=>{controller.abort();onGet=null;};
  await assert.rejects(cancelled.readWindow({window:[1,1,7,8],bands:selected},{signal:controller.signal}),{code:'ABORTED'});
  await cancelled.close();
  console.log(JSON.stringify({ok:true,format:path.endsWith('.skv')?'skv':'tiff',bands:40,groupedVerifications,controlVerifications,conditional:true,mutationRejected:true,cancellationRejected:true,cacheBytes:0}));
} finally {
  await engine.close();
  await new Promise(resolve=>server.close(resolve));
}
