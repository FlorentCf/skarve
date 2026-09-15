#!/usr/bin/env node
/** Installed min-only queries cannot fail because an unrequested sum overflows. */
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {dirname,join} from 'node:path';
import Skarve from '@skarve/engine';

const [fixturePath,backend='native']=process.argv.slice(2);
assert(fixturePath,'Usage: product_api_metrics.mjs FIXTURE.json [native|exactextract]');
const fixture=JSON.parse(readFileSync(fixturePath,'utf8'));
const path=join(dirname(fixturePath),fixture.source);
const minimum=band=>{
  assert.equal(band.min,fixture.min);
  assert(!Object.hasOwn(band,'fractional_sum'));
  assert(!Object.hasOwn(band,'coverage_weighted_mean'));
};
const sk=new Skarve();
try {
  const source=await sk.infuse(path);
  try {
    const min=await source.carve({zone:fixture.zone,metrics:['min'],backend});
    minimum(min.bands[0]);
    if(backend==='exactextract') {
      assert.equal(min.work.bridge_abi_version,2);assert.equal(min.work.statistics_mask,8);
    }
    const max=await source.carve({zone:fixture.zone,metrics:['max','support'],backend});
    assert.equal(max.bands[0].max,fixture.min);assert.equal(max.bands[0].covered_cell_equivalents,4);
    assert(!Object.hasOwn(max.bands[0],'fractional_sum'));
    await assert.rejects(source.carve({zone:fixture.zone,metrics:['sum'],backend}),/overflow|nonfinite/i);
    minimum((await source.carve({zone:fixture.zone,metrics:['min'],backend})).bands[0]);
  } finally {await source.close();}
  const job={zones:['a','b'].map(id=>({id,version:'1',geometry:fixture.zone})),
    slices:[{id:'slice',spec:{location:path},bands:[0]}],crs:fixture.crs,metrics:['min'],backend};
  let rows=0,last;
  for await(const page of sk.cleave(job,{maxRows:1})) {
    assert(page.rows.length<=1);
    for(const row of page.rows){minimum(row.bands[0]);rows++;}
    last=page;
  }
  assert.equal(rows,2);assert(last.complete);
  console.log(JSON.stringify({passed:true,backend,single_min:fixture.min,batch_rows:rows,
    requested_sum_overflow_rejected:true,unrequested_sum_not_required:true}));
} finally {await sk.close();}
