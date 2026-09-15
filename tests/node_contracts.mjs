#!/usr/bin/env node
/** Installed-only source and typed-buffer contract checks on generated inputs. */
import assert from 'node:assert/strict';
import {readFileSync,mkdirSync,copyFileSync,appendFileSync} from 'node:fs';
import {dirname,join} from 'node:path';
import Skarve,{bulkMemoryStats} from '@skarve/engine';

const [fixturePath,scratch]=process.argv.slice(2);
assert(fixturePath&&scratch);mkdirSync(scratch,{recursive:false});
for(const name of ['SKARVE_LIBRARY','RASTER_ENGINE_LIB','RASTER_ENGINE_LIBRARY','PYTHONPATH'])assert(!process.env[name]);
const fixture=JSON.parse(readFileSync(fixturePath,'utf8')),folder=dirname(fixturePath),checks=[];
const passed=(name,details={})=>checks.push({name,passed:true,...details});
const rejected=async(name,promise)=>{await assert.rejects(promise);passed(name);};
const one=(values,options={})=>[{bands:[{id:17,values,...options}]}];
const statistics=['sum','support','mean','min','max','count'];
const checkBand=(actual,expected)=>{
  assert.equal(actual.band,expected.band);
  for(const key of ['fractional_sum','covered_cell_equivalents','coverage_weighted_mean','min','max','valid_cell_count']) {
    if(expected[key]===null)assert.equal(actual[key],null);
    else if(key==='valid_cell_count')assert.equal(actual[key],expected[key]);
    else assert(Math.abs(actual[key]-expected[key])<=1e-8+1e-10*Math.abs(expected[key]),key);
  }
};
const engine=new Skarve();
try {
  const source=await engine.openSource(join(folder,fixture.sources.original.file),{id:'source'});
  try {
    for(const name of ['small','thin','outside']) {
      const result=await source.measure(fixture.geometries[name],{crs:fixture.crs,statistics});
      assert.equal(result.bands.length,3);
      result.bands.forEach((band,i)=>checkBand(band,fixture.sources.original.expected[name][i]));
      if(name==='thin')assert(result.bands.some(b=>b.covered_cell_equivalents>0));
    }
    passed('source-owned-mask-scale-offset-thin-outside');
    await rejected('source-unknown-reducer-rejected',source.measure(fixture.geometries.small,{crs:fixture.crs,statistics:['unknown']}));
    checkBand((await source.measure(fixture.geometries.small,{crs:fixture.crs,bands:[0],statistics})).bands[0],fixture.sources.original.expected.small[0]);
    passed('source-error-cleanup-and-reuse');
  } finally {await source.close();}
  assert.throws(()=>source.inspect(),/closed/i);passed('closed-source-wrapper-rejected');
  const mutated=join(scratch,'mutated.tif');copyFileSync(join(folder,fixture.sources.original.file),mutated);
  const changed=await engine.openSource(mutated,{id:'changed'});
  try {
    await changed.measure(fixture.geometries.small,{crs:fixture.crs});
    appendFileSync(mutated,'changed-generated-fixture');
    await rejected('source-mutation-rejected',changed.measure(fixture.geometries.small,{crs:fixture.crs}));
  } finally {await changed.close();}
  for(const Type of [Float32Array,Float64Array])for(const count of [36,40]) {
    const backing=new Type([99,1.25,-2,0,3.75,99]),values=backing.subarray(1,5);
    const windows=[{bands:Array.from({length:count},(_,i)=>({id:100+i,values}))}];
    const strict=await engine.bulkReduce(windows,{reducers:['sum','min','max','mean']});
    const ordered=await engine.bulkReduce(windows,{policy:'hm_demographics_ordered_v1',reducers:['sum','min','max','mean']});
    assert(strict.bands.every(b=>b.sum===3&&b.valid_count===4&&b.min===-2&&b.mean===.75));
    assert(ordered.bands.every(b=>b.sum===5&&b.valid_count===3&&b.min===0&&b.mean===5/3));
    assert.deepEqual(strict.bands.map(b=>b.id),Array.from({length:count},(_,i)=>100+i));
    assert.equal(strict.metadata.copied_bytes,4*Type.BYTES_PER_ELEMENT*count);
    passed(`typed-${Type.name}-${count}-strict-ordered-and-band-identities`);
  }
  const threshold=new Float32Array([2**24,.5-2**-25,...Array(32).fill(2**-30)]);
  const strict=await engine.bulkReduce(one(threshold));
  const ordered=await engine.bulkReduce(one(threshold),{policy:'hm_demographics_ordered_v1'});
  let expected=0;for(const x of threshold)expected+=x;
  assert.equal(strict.bands[0].sum,16777216.5);assert.equal(ordered.bands[0].sum,expected);
  assert.notEqual(strict.bands[0].sum,ordered.bands[0].sum);
  const split=[one(new Float64Array([1e16]))[0],one(new Float64Array([1,1]))[0]];
  assert.equal((await engine.bulkReduce(split,{policy:'hm_demographics_ordered_v1'})).bands[0].sum,10000000000000002);
  passed('ordered-window-fold-and-rounding-threshold');
  const masked=one(new Float64Array([10,-2,NaN,-999,Infinity,0,8]),{validity:new Uint8Array([1,1,1,1,1,1,0]),nodata:-999});
  masked[0].selection=new Uint32Array([0,1,2,3,4,5,6,0]);
  const row=(await engine.bulkReduce(masked,{policy:'hm_demographics_ordered_v1'})).bands[0];
  assert.equal(row.sum,20);assert.equal(row.valid_count,3);
  assert.equal(row.excluded_mask,1);assert.equal(row.excluded_nodata,1);assert.equal(row.excluded_nonfinite,2);assert.equal(row.excluded_negative,1);
  await rejected('strict-valid-nonfinite-rejected',engine.bulkReduce(masked));
  passed('typed-selection-order-and-mask-classification');
  const values=new Float32Array([1,2,3]);
  for(const options of [{maxPayloadBytes:4},{maxContributions:1},{policy:'unknown'},{reducers:['sum','sum']},{reducers:['variance']}]) {
    await rejected('typed-limit-or-contract-rejected',engine.bulkReduce(one(values),options));
    assert.equal(bulkMemoryStats().owned_bytes,0);
  }
  await rejected('typed-selection-bounds-rejected',engine.bulkReduce([{bands:[{id:0,values}],selection:new Uint32Array([3])}]));
  await rejected('typed-arithmetic-overflow-rejected',engine.bulkReduce(one(new Float64Array([1e308,1e308]))));
  await rejected('typed-shared-buffer-rejected',engine.bulkReduce(one(new Float32Array(new SharedArrayBuffer(8)))));
  const mismatched=[one(values)[0],{bands:[{id:18,values}]}];
  await rejected('typed-window-band-identity-mismatch',engine.bulkReduce(mismatched));
  assert.equal((await engine.bulkReduce(one(values))).bands[0].sum,6);
  passed('typed-failure-cleanup-and-reuse');
  const originalStringify=JSON.stringify;
  JSON.stringify=()=>{throw new Error('Typed pixels must not use JSON');};
  try {assert.equal((await engine.bulkReduce(one(values))).bands[0].sum,6);}
  finally {JSON.stringify=originalStringify;}
  passed('typed-path-does-not-serialize-pixels');
  const larger=[{bands:Array.from({length:40},(_,id)=>({id,values:new Float32Array(120000).fill(1)}))}];
  const control=new AbortController(),pending=engine.bulkReduce(larger,{signal:control.signal});control.abort();
  await assert.rejects(pending,error=>error.code==='ABORTED');
  assert.equal(bulkMemoryStats().owned_bytes,0);
  assert.equal((await engine.bulkReduce(one(values))).bands[0].sum,6);
  passed('async-abort-drains-and-reuses-session');
  const closing=engine.bulkReduce(larger),rejection=assert.rejects(closing,error=>error.code==='ABORTED');
  await engine.close();await rejection;assert.equal(bulkMemoryStats().owned_bytes,0);
  passed('close-during-call-cancels-and-drains');
} finally {await engine.close();}
assert.equal(bulkMemoryStats().owned_bytes,0);
console.log(JSON.stringify({schema:1,passed:true,interface:'Node',checks,check_count:checks.length,
  new_external_network_requests:0,owned_snapshot_bytes_after:0}));
