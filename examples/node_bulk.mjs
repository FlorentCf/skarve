#!/usr/bin/env node
/** Caller-selected typed values use an explicit contract independent of sources. */
import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import Skarve,{bulkMemoryStats} from '@skarve/engine';

export async function run() {
  const engine=new Skarve(),cases=[];
  try {
    for(const count of [36,40]) {
      const values=new Float32Array([1.25,-2,0,3.75]);
      const windows=[{bands:Array.from({length:count},(_,band)=>({id:100+band,values})),
        selection:new Uint32Array([0,1,2,3])}];
      const strict=await engine.bulkReduce(windows);
      const ordered=await engine.bulkReduce(windows,{policy:'hm_demographics_ordered_v1'});
      assert(strict.bands.every(b=>b.sum===3&&b.valid_count===4));
      assert(ordered.bands.every(b=>b.sum===5&&b.valid_count===3));
      assert.deepEqual(strict.bands.map(b=>b.id),Array.from({length:count},(_,b)=>100+b));
      assert.equal(bulkMemoryStats().owned_bytes,0);
      cases.push({bands:count,strict_sum:3,ordered_sum:5,copied_bytes:strict.metadata.copied_bytes});
    }
    return {interface:'Node',cases,source_identity_claimed:false};
  } finally {await engine.close();}
}
if(process.argv[1]===fileURLToPath(import.meta.url))console.log(JSON.stringify(await run()));
