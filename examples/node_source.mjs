#!/usr/bin/env node
/** Installed direct source, optional summary index and genuine paged batch. */
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {dirname,join} from 'node:path';
import {fileURLToPath} from 'node:url';
import Skarve from '@skarve/engine';

const statistics=['sum','support','mean','min','max','count'];
export function makeJob(folder,fixture) {
  return {zones:['small','overlap','other'].map(id=>({id,version:'1',geometry:fixture.geometries[id]})),
    slices:['original','date-b'].flatMap(source=>[2,0,1].map(band=>({id:`${source}-band${band}`,
      time:`synthetic-${source}`,spec:{location:join(folder,fixture.sources[source].file)},bands:[band]}))),
    crs:fixture.crs,tile_edge:32,
    budget:{working_bytes:96*1024**2,geometry_bytes:8*1024**2,tile_bytes:8*1024**2,
      output_bytes:1024**2,max_windows:64,decoded_bytes:8*1024**2,max_contributions:1024**2,workers:1},
    options:{statistics}};
}
export async function run(fixturePath,indexPath) {
  const fixture=JSON.parse(readFileSync(fixturePath,'utf8')),folder=dirname(fixturePath);
  const engine=new Skarve();
  try {
    await engine.configureFileCache(1024**2);
    const source=await engine.infuse(join(folder,fixture.sources.original.file),{id:'elevation'});
    let direct,built;
    try {
      direct=await source.carve({zone:fixture.geometries.small,crs:fixture.crs,bands:[0],metrics:statistics});
      built=await source.ward(indexPath,{boundary_source:'original',tile_edge:16});
      const index=await source.openIndex(join(indexPath,'summary.rsi'),{expected_build_id:built.build_id});
      try {
        const indexed=await index.measure(fixture.geometries.small,{crs:fixture.crs,bands:[0],statistics});
        assert.deepEqual(direct.bands,indexed.bands);
      } finally {await index.close();}
    } finally {await source.close();}
    let rows=0,last;
    for await (const page of engine.cleave(makeJob(folder,fixture),{maxRows:2})) {
      assert(page.rows.length<=2);rows+=page.rows.length;last=page;
      // Consume this bounded page before requesting the next one.
    }
    assert(last.complete&&rows===18);
    return {interface:'Node',direct:direct.bands,indexed_equal:true,batch_rows:rows,
      batch_metrics:last.metrics,index_bytes:built.summary_bytes,duplicate_raw_bytes:built.duplicate_raw_bytes};
  } finally {await engine.close();}
}
if(process.argv[1]===fileURLToPath(import.meta.url)) {
  const [fixture,index]=process.argv.slice(2);assert(fixture&&index,'Usage: node node_source.mjs FIXTURE.json NEW_INDEX');
  console.log(JSON.stringify(await run(fixture,index)));
}
