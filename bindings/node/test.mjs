import test from 'node:test';
import assert from 'node:assert/strict';
import { RasterEngine } from './index.mjs';
const grid={width:2,height:2,transform:[0,1,0,2,0,-1],crs:'LOCAL'};
const rectangle=(x0,y0,x1,y1)=>({type:'Polygon',coordinates:[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]});

test('native numerical semantics, prepared and shared plan, resource lifetime',async()=>{
 const e=new RasterEngine();
 try{
  await e.open('r',{grid,bands:[{values:[2,-4,6,8],valid:[true,true,true,false],unit:'u'},{values:[4,-8,12,16]}]});
  const half=await e.measure('r',rectangle(0,1,.5,2),{crs:'LOCAL',bands:[0]});
  assert.equal(half.bands[0].fractional_sum,1);assert.equal(half.bands[0].covered_cell_equivalents,.5);
  const all=await e.measure('r',rectangle(0,0,2,2),{crs:'LOCAL'});
  assert.equal(all.bands[0].fractional_sum,4);assert.equal(all.bands[0].missing_cell_equivalents,1);assert.equal(all.bands[0].min,-4);
  await e.compile('r','p',rectangle(0,0,2,2),'LOCAL');
  const planned=await e.measure('r','p');assert.deepEqual(planned.bands,all.bands);
  await e.prepare('r');const prepared=await e.measure('r','p');assert.deepEqual(prepared.bands,all.bands);
  assert.ok(e.lastTiming.total>=0);assert.equal((await e.stats()).sources,1);
  await assert.rejects(e.measure('r',rectangle(0,0,2,2),{crs:'WRONG'}),/CRS/i);
  await e.closeSource('r');assert.equal((await e.stats()).sources,0);
 }finally{await e.close();}
 await e.close();assert.equal(e.closed,true);await assert.rejects(e.stats(),{code:'CLOSED'});
});

test('native errors do not poison session; pre-abort and concurrency bounds',async()=>{
 const e=new RasterEngine();
 try{
  await assert.rejects(e.request({op:'not-an-operation'}));
  const controller=new AbortController();controller.abort();
  await assert.rejects(e.stats({signal:controller.signal}),{code:'ABORTED'});
  const work=e.stats();await assert.rejects(e.stats(),{code:'BUSY'});await work;
  assert.equal((await e.stats()).sources,0);
 }finally{await e.close();}
});

test('native execution leaves event loop responsive and cancellation drains',async()=>{
 const e=new RasterEngine();
 try{
  const n=512;await e.open('large',{grid:{width:n,height:n,transform:[0,1,0,n,0,-1],crs:'LOCAL'},bands:[{values:Array(n*n).fill(1)}]});
  const ring=Array.from({length:512},(_,i)=>[256+245*Math.cos(i*Math.PI/256),256+245*Math.sin(i*Math.PI/256)]);ring.push(ring[0]);
  const controller=new AbortController();let ticks=0;
  const interval=setInterval(()=>{ticks++;controller.abort();},2);
  try{await assert.rejects(e.measure('large',{type:'Polygon',coordinates:[ring]},{crs:'LOCAL',strategy:'direct',signal:controller.signal}),{code:'ABORTED'});}finally{clearInterval(interval);}
  assert.ok(ticks>0,'main-thread timer runs during native work');assert.equal(e.busy,false);
  assert.equal((await e.stats()).sources,1);
 }finally{await e.close();}
});
