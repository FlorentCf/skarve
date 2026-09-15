import test from 'node:test';
import assert from 'node:assert/strict';
import { RasterEngine } from './index.mjs';
const box = (a,b,c,d) => ({type:'Polygon',coordinates:[[[a,b],[c,b],[c,d],[a,d],[a,b]]]});
const raster = t => ({grid:{width:128,height:128,transform:[0,1,0,128,0,-1],crs:'LOCAL'},bands:[{values:Array.from({length:128*128},(_,i)=>(i+t)%19),valid:Array.from({length:128*128},(_,i)=>(i+t)%11!==0)}]});
test('native shared pages reuse geometry across actual distinct values and permit early iterator close', async () => {
  const engine=new RasterEngine();
  try {
    await engine.open('a',raster(0));await engine.open('b',raster(1));
    const job={zones:[{id:'one',version:'1',geometry:box(.5,.5,127.5,127.5)},{id:'two',version:'1',geometry:box(0,0,128,128)}],slices:[{id:'first',source:'a'},{id:'second',source:'b'}],crs:'LOCAL',tile_edge:64,options:{statistics:['sum','support','mean']}};
    const pages=[];for await(const p of engine.batchPages(job,{maxRows:1}))pages.push(p);
    assert.equal(pages.length,4);assert.equal(pages.at(-1).metrics.windows_read,8);
    assert.equal(pages.at(-1).metrics.geometry_compilations,2);assert.equal(pages.at(-1).metrics.geometry_cache_hits,2);
    assert.equal(new Set(pages.flatMap(p=>p.rows.map(r=>r.result_id))).size,4);
    const resumed=[];for await(const r of engine.batch(job,{checkpoint:pages[0].checkpoint}))resumed.push(r);
    assert.deepEqual(resumed.map(r=>r.result_id),pages.slice(1).flatMap(p=>p.rows.map(r=>r.result_id)));
    for await(const _ of engine.batchPages(job,{id:'early',maxRows:1}))break;
    await assert.rejects(engine.request({op:'job_info',id:'early'}),/unknown job/);
  } finally {await engine.close();}
});
test('trusted expression fails zero denominators and preserves mean-of-ratios semantics', async () => {
  const engine=new RasterEngine();
  try {
    await engine.open('r',{grid:{width:3,height:1,transform:[0,1,0,1,0,-1],crs:'LOCAL'},bands:[{values:[8,3,0]},{values:[2,1,0]}]});
    const job={zones:[{id:'z',version:'1',geometry:box(0,0,3,1)}],slices:[{id:'t',source:'r'}],crs:'LOCAL',expression:{op:'normalized_difference',left:{op:'band',band:0},right:{op:'band',band:1}},options:{statistics:['mean','support']}};
    const rows=[];for await(const r of engine.batch(job))rows.push(r);
    assert.equal(rows[0].bands[0].covered_cell_equivalents,2);
    assert.ok(Math.abs(rows[0].bands[0].coverage_weighted_mean-.55)<1e-15);
  } finally {await engine.close();}
});
