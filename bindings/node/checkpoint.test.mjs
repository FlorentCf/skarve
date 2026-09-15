import test from 'node:test';
import assert from 'node:assert/strict';
import { RasterEngine } from './index.mjs';

test('periodic checkpoints preserve rows and resume while final pages always emit', async () => {
  const engine = new RasterEngine();
  try {
    await engine.open('r',{grid:{width:2,height:1,transform:[0,1,0,1,0,-1],crs:'LOCAL'},bands:[{values:[1,3]}]});
    const geometry={type:'Polygon',coordinates:[[[0,0],[2,0],[2,1],[0,1],[0,0]]]};
    const job={zones:[0,1,2].map(i=>({id:String(i),version:'1',geometry})),slices:[0,1,2].map(i=>({id:String(i),source:'r'})),crs:'LOCAL',tile_edge:32,options:{statistics:['sum','support','mean']}};
    const baseline=[];for await(const p of engine.batchPages(job,{maxRows:1}))baseline.push(p);
    const pages=[];for await(const p of engine.batchPages(job,{maxRows:1,checkpointInterval:4}))pages.push(p);
    assert.ok(baseline.every(p=>p.checkpoint));
    assert.deepEqual(pages.map(p=>'checkpoint' in p),[false,false,false,true,false,false,false,true,true]);
    assert.deepEqual(pages.flatMap(p=>p.rows),baseline.flatMap(p=>p.rows));
    const resumed=[];for await(const r of engine.batch(job,{checkpoint:pages[3].checkpoint,checkpointInterval:4}))resumed.push(r);
    assert.deepEqual(resumed,baseline.slice(4).flatMap(p=>p.rows));
    assert.equal(pages.at(-1).checkpoint.next_row,9);
    assert.equal(pages.at(-1).metrics.windows_read,baseline.at(-1).metrics.windows_read);
    for(const checkpointInterval of [0,-1,1.5,true,Number.MAX_SAFE_INTEGER+1]) {
      await assert.rejects(async()=>{for await(const _ of engine.batchPages(job,{id:'invalid',checkpointInterval})){}},/positive safe integer/);
      await assert.rejects(engine.request({op:'job_info',id:'invalid'}),/unknown job/);
    }
  } finally {await engine.close();}
});
