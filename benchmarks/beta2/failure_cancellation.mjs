#!/usr/bin/env node
// Installed consumer companion to failure_cancellation.py; controlled I/O,
// not a CPU-only latency or hard cancellation benchmark.
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {get} from 'node:http';
import Skarve from '@skarve/engine';

const [url,folder,repetitions,...backends]=process.argv.slice(2);
const fixture=JSON.parse(readFileSync(join(folder,'fixture.json'),'utf8'));
const zone=fixture.remote.geometries['hot-a'];
const expected=fixture.remote.expected['hot-a'][0].fractional_sum;
const policies={native:'native_grid_planar_fractional',exactextract:'exactextract_fractional_v030'};
const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));
// Core HTTP avoids undici's Wasm address-space reservation in the 2 GiB
// consumer envelope; neither path is part of Skarve's source access.
const control=action=>new Promise((resolve,reject)=>{
  const request=get(url+'/control/'+action,{agent:false,timeout:5000},response=>{
    if(response.statusCode!==200){response.resume();reject(new Error('Control request failed'));return;}
    const chunks=[];let bytes=0;
    response.on('data',chunk=>{bytes+=chunk.length;if(bytes>4096)request.destroy(new Error('Control response limit'));else chunks.push(chunk);});
    response.on('end',()=>{try{resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));}catch(error){reject(error);}});
    response.on('error',reject);
  });
  request.on('timeout',()=>request.destroy(new Error('Control timeout')));request.on('error',reject);
});
const rows=[];
for(const backend of backends)for(let repeat=0;repeat<Number(repetitions);repeat++){
  const sk=new Skarve();
  try {
    const source=await sk.infuse({location:url+'/source.tif',bands:[0],http:{allow_http:true,
      cache_bytes:0,max_requests:128,max_range_bytes:1<<20,max_download_bytes:8<<20}});
    try {
      const before=(await control('state')).gets;
      const start=performance.now();let failure;
      try {await source.carve({zone,metrics:['sum'],backend,numerical_policy:policies[backend==='native'?'exactextract':'native']});}
      catch(error){failure=error.message;}
      const rejected=performance.now();assert.match(failure,/polic/i);
      assert.equal((await control('state')).gets,before);
      await control('arm');const abort=new AbortController();const queryStart=performance.now();
      let settled=false;
      const pending=source.carve({zone,metrics:['sum'],backend,signal:abort.signal}).then(
        value=>{settled=true;return {value};},error=>{settled=true;return {error};});
      const deadline=performance.now()+4000;
      while(!(await control('state')).entered){
        assert(!settled,'Query completed before active range-read proof');
        assert(performance.now()<deadline,'No active source read');await pause(1);
      }
      assert(!settled);const cancelled=performance.now();abort.abort();const initiated=performance.now();
      await pause(25);const stillActive=!settled;const releaseAt=performance.now();await control('release');
      const outcome=await pending;const drained=performance.now();
      assert(stillActive,'Returned before held source read drained');assert.equal(outcome.error?.code,'ABORTED');
      assert.equal(sk.busy,false);const reuseStart=performance.now();let sameSourceError=null;
      try {await source.carve({zone,metrics:['sum'],backend});}
      catch(error){sameSourceError=error.message;}
      const sameSourceEnd=performance.now();
      await source.close();
      const reopened=await sk.infuse({location:url+'/source.tif',bands:[0],http:{allow_http:true,
        cache_bytes:0,max_requests:128,max_range_bytes:1<<20,max_download_bytes:8<<20}});
      let result;
      try {result=await reopened.carve({zone,metrics:['sum'],backend});}
      finally {await reopened.close();}
      const reuseEnd=performance.now();
      assert.equal(result.provenance.selected_backend,backend);
      assert(Math.abs(result.bands[0].fractional_sum-expected)<1e-8);
      rows.push({runtime:'node',backend,repeat,rejection_ms:rejected-start,rejection_error:failure,rejection_source_gets:0,
        initiate_cancel_ms:initiated-cancelled,cancel_to_drain_ms:drained-cancelled,
        release_to_drain_ms:drained-releaseAt,query_total_until_drain_ms:drained-queryStart,
        controlled_hold_after_cancel_ms:releaseAt-cancelled,active_read_observed:true,
        still_active_before_release:stillActive,cancel_outcome:outcome.error.code,
        same_source_reuse_ms:sameSourceEnd-reuseStart,same_source_reuse_error:sameSourceError,
        session_reuse_with_new_source_ms:reuseEnd-sameSourceEnd,session_reuse_answer_correct:true});
    } finally {await source.close();}
  } finally {await sk.close();}
}
console.log(JSON.stringify({schema:1,runtime:'node',rows}));
