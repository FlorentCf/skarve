import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,readFileSync} from 'node:fs';
import {join,dirname} from 'node:path';
import {tmpdir} from 'node:os';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
import Skarve, {Engine,RasterEngine} from '../bindings/node/index.mjs';

const root=dirname(dirname(fileURLToPath(import.meta.url)));
const scratch=mkdtempSync(join(tmpdir(),'skarve-product-api-'));
const fixtureFolder=join(scratch,'data');
execFileSync(process.env.SKARVE_TEST_PYTHON || 'python3',[join(root,'examples/generate_fixtures.py'),fixtureFolder]);
const fixture=JSON.parse(readFileSync(join(fixtureFolder,'fixture.json'),'utf8'));
const path=join(fixtureFolder,'original.tif');
const metrics=['sum','support','mean','min','max'];

test('Skarve aliases preserve registered band mapping and independent source handles',async()=>{
  assert.equal(Skarve,Engine);assert.equal(Skarve,RasterEngine);
  const sk=new Skarve();
  try {
    const source=await sk.infuse({location:path,bands:[2,0]});
    const other=await sk.infuse(path);
    assert.notEqual(source.id,other.id);
    const branded=await source.carve({zone:fixture.geometries.small,bands:[0],metrics});
    const old=await source.measure(fixture.geometries.small,{bands:[0],statistics:metrics,crs:fixture.crs});
    const mapped=await other.carve({zone:fixture.geometries.small,bands:[2],metrics});
    assert.deepEqual(branded.bands,old.bands);
    assert.equal(branded.bands[0].fractional_sum,mapped.bands[0].fractional_sum);
    await assert.rejects(source.carve({zone:fixture.geometries.small,crs:'EPSG:4326'}),/CRS/);
    assert.throws(()=>source.carve({zone:fixture.geometries.small,metrics,statistics:metrics}),/not both/);
    const built=await source.ward(join(scratch,'index'),{boundary_source:'original',tile_edge:16});
    assert.equal(built.duplicate_raw_bytes,0);
    await source.close();await other.close();
    assert.throws(()=>source.carve({zone:fixture.geometries.small}),/closed/);
  } finally {await sk.close();}
});

test('cleave streams actual shared execution and closes an early iterator',async()=>{
  const sk=new Skarve();
  try {
    const job={zones:['small','overlap'].map(id=>({id,version:'1',geometry:fixture.geometries[id]})),
      slices:['first','second'].map(id=>({id,spec:{location:path},bands:[1,0]})),crs:fixture.crs,metrics};
    const pages=[];
    for await(const page of sk.cleave(job,{maxRows:1}))pages.push(page);
    assert.equal(pages.length,4);assert(pages.at(-1).complete);
    assert(pages.at(-1).metrics.geometry_cache_hits>0);
    assert.equal(new Set(pages.flatMap(page=>page.rows.map(row=>row.result_id))).size,4);
    assert(!Object.hasOwn(job,'options'));
    for await(const _ of sk.cleave(job,{id:'early',maxRows:1}))break;
    await assert.rejects(sk.request({op:'job_info',id:'early'}),/unknown job/);
  } finally {await sk.close();}
});

test('carve keeps async admission, cancellation and session draining',async()=>{
  const sk=new Skarve();
  const source=await sk.infuse(path);
  try {
    const aborted=new AbortController();aborted.abort();
    await assert.rejects(source.carve({zone:fixture.geometries.whole,signal:aborted.signal}),{code:'ABORTED'});
    const pending=source.carve({zone:fixture.geometries.whole});
    await assert.rejects(source.carve({zone:fixture.geometries.whole}),{code:'BUSY'});
    await pending;
    const active=source.carve({zone:fixture.geometries.whole});
    const drained=assert.rejects(active,{code:'ABORTED'});
    await sk.close();await drained;
    assert.equal(sk.busy,false);assert.equal(sk.closed,true);
  } finally {await sk.close();}
});
