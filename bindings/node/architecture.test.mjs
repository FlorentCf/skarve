import test from 'node:test';
import assert from 'node:assert/strict';
import {RasterEngine} from './index.mjs';
const geometry={type:'Polygon',coordinates:[[[0,0],[4,0],[4,1],[0,1],[0,0]]]};
test('ordinary Node methods expose preparation, statistic dependencies and capability fallback',async()=>{
  const engine=new RasterEngine();
  try {
    await engine.open('r',{grid:{width:4,height:1,transform:[0,1,0,1,0,-1],crs:'LOCAL'},bands:[{values:[1,-2,3,4]}]});
    for(const backend of ['row_blocks','hierarchy','cumulative_full','cumulative_blocked']) {
      await engine.prepare('r',{backend});
      const result=await engine.measure('r',geometry,{crs:'LOCAL',backend,statistics:['sum','mean']});
      assert.equal(result.bands[0].fractional_sum,6);
      assert.equal(result.bands[0].coverage_weighted_mean,1.5);
      assert.equal('min' in result.bands[0],false);
    }
    const fallback=await engine.measure('r',geometry,{crs:'LOCAL',backend:'cumulative_full',statistics:['min','max']});
    assert.equal(fallback.bands[0].min,-2);
    assert.match(fallback.selection_reason,/requires scanline/);
    await assert.rejects(engine.measure('r',geometry,{crs:'LOCAL',statistics:['sum','sum']}),/duplicate/);
  } finally {await engine.close();}
});
