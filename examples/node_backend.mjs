#!/usr/bin/env node
/** Single source query and paged batch share the same optional backend contract. */
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {dirname,join} from 'node:path';
import {fileURLToPath} from 'node:url';
import Skarve from '@skarve/engine';

const metrics=['sum','support','mean','min','max'];
export async function run(fixturePath,backend='native',acceptedPolicies) {
  const fixture=JSON.parse(readFileSync(fixturePath,'utf8')),folder=dirname(fixturePath);
  const selection={backend,...(acceptedPolicies?{accepted_policies:acceptedPolicies}:{})};
  const sk=new Skarve();
  try {
    const source=await sk.infuse(join(folder,fixture.sources.original.file));
    let single;
    try {single=await source.carve({zone:fixture.geometries.small,bands:[0,2],metrics,...selection});}
    finally {await source.close();}
    const job={zones:['small','overlap','thin','outside'].map(id=>({id,version:'1',geometry:fixture.geometries[id]})),
      slices:['original','date-b'].map(id=>({id,spec:{location:join(folder,fixture.sources[id].file)},bands:[0,2]})),
      crs:fixture.crs,metrics,...selection};
    let rows=0,final;
    for await(const page of sk.cleave(job,{maxRows:2})) {
      // Consume the bounded page before requesting more; do not collect all rows.
      rows+=page.rows.length;final=page;
    }
    assert.equal(rows,8);assert(final.complete);
    return {single,batch_rows:rows,batch_metrics:final.metrics,batch_provenance:final.provenance??null};
  } finally {await sk.close();}
}
if(process.argv[1]===fileURLToPath(import.meta.url)) {
  const [fixture,backend='native',accepted]=process.argv.slice(2);
  assert(fixture,'Usage: node node_backend.mjs FIXTURE.json [native|exactextract|auto] [POLICIES_CSV]');
  console.log(JSON.stringify(await run(fixture,backend,accepted?.split(','))));
}
