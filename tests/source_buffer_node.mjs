import assert from 'node:assert/strict';
const { Skarve } = await import(process.env.SKARVE_TEST_MODULE ?? '../bindings/node/index.mjs');
const file = process.argv[2];
assert(file, 'Pass a generated TIFF/SKV fixture path');
const engine = new Skarve();
const source = await engine.infuse(file);
assert(source.metadata.rawMetadata.bands.length >= 3);
assert.equal(source.metadata.rawMetadata.maxWindowBands, 64);
const result = await source.readWindow({window:[1,1,7,8],bands:[2,0]});
assert.equal(result.bands[0].values.constructor, Float32Array);
assert.equal(result.bands[0].values[0], 0);
assert.equal(result.bands[0].mask[8], 0);
assert.equal(result.bands[0].metadata.scaleBits, '4000000000000000');
assert.equal(result.bands[0].metadata.offsetBits, '4008000000000000');
assert.equal(result.bands[0].values[1], 2*19*17+19);
assert.equal(result.bands[1].values[1], 19);
assert.equal(result.bands[0].values.buffer, result.bands[1].mask.buffer);
assert.equal(result.reservedBytes.output,result.byteLength);
assert.throws(()=>source.readWindow({window:[0,0,1,1],bands:[0,0]}),/Invalid bands/);
let wideChecks = 0;
for (const count of [37, 40]) {
  if (source.metadata.rawMetadata.bands.length < count) continue;
  const selected = Array.from({length:count}, (_,i)=>count-1-i);
  const complete = await source.readWindow({window:[1,1,7,8],bands:selected});
  assert.equal(complete.bands.length,count);
  const asBytes = value => Buffer.from(value.buffer,value.byteOffset,value.byteLength);
  for (let start=0;start<count;start+=20) {
    const control = await source.readWindow({window:[1,1,7,8],bands:selected.slice(start,start+20)});
    for (const [i, reference] of control.bands.entries()) {
      const actual=complete.bands[start+i];
      assert.equal(actual.sourceBand,selected[start+i]);
      assert.deepEqual(asBytes(actual.values),asBytes(reference.values));
      assert.deepEqual(actual.mask,reference.mask);
      assert.deepEqual(actual.metadata,reference.metadata);
    }
  }
  wideChecks += count;
}
assert.throws(()=>source.readWindow({window:[0,0,17,19],bands:[0],maxBytes:1}),/budget/);
await assert.rejects(source.readWindow({window:[0,0,17,19],bands:[0],workingBytes:1024}),/budget/);
const controller = new AbortController();controller.abort();
assert.throws(()=>source.readWindow({window:[0,0,1,1],bands:[0]},{signal:controller.signal}),{code:"ABORTED"});
const pending=source.readWindow({window:[0,0,17,19],bands:[0]});
assert.throws(()=>source.readWindow({window:[0,0,1,1],bands:[0]}),/busy/i);
await pending;
await source.close();await engine.close();
assert.equal(result.bands[0].values[1], 2*19*17+19, 'result owns its storage after source/session close');
console.log(JSON.stringify({ok:true,checks:16,wideChecks,file}));
