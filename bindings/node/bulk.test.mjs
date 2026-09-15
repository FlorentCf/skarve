import test from 'node:test';
import assert from 'node:assert/strict';
import { RasterEngine, bulkMemoryStats } from './index.mjs';

const withEngine = async fn => { const engine = new RasterEngine(); try { await fn(engine); } finally { await engine.close(); assert.equal(bulkMemoryStats().owned_bytes, 0); } };
const one = (values, rest = {}) => [{ bands: [{ id: 17, values, ...rest }] }];
const hm = { policy: 'hm_demographics_ordered_v1', reducers: ['sum','min','max','mean'] };

test('bulk f32/f64, 1/18/20/36/40 bands and offset views preserve source dtype', async () => withEngine(async engine => {
  for (const Type of [Float32Array, Float64Array]) for (const count of [1,18,20,36,40]) {
    const backing = new Type([999, 1.25, 2.5, -4, 999]);
    const values = backing.subarray(1,4);
    const result = await engine.bulkReduce([{ bands: Array.from({ length: count }, (_, id) => ({ id, values })) }]);
    assert.equal(result.bands.length, count);
    assert.ok(result.bands.every(x => x.sum === -0.25 && x.valid_count === 3));
    assert.equal(result.metadata.copied_bytes, count * 3 * Type.BYTES_PER_ELEMENT);
    assert.deepEqual([...backing], [999,1.25,2.5,-4,999]);
  }
}));

test('ordered HM round-threshold and window partial semantics are isolated', async () => withEngine(async engine => {
  const values = new Float32Array([2**24, .5 - 2**-25, ...Array(32).fill(2**-30)]);
  const strict = await engine.bulkReduce(one(values));
  const ordered = await engine.bulkReduce(one(values), hm);
  let expected = 0; for (const value of values) expected += value;
  assert.equal(ordered.bands[0].sum, expected);
  assert.equal(Math.round(strict.bands[0].sum), 16777217);
  assert.equal(Math.round(ordered.bands[0].sum), 16777216);
  const windows = [one(values.subarray(0,2))[0], one(values.subarray(2))[0]];
  let first = 0, second = 0;
  for (const x of values.subarray(0,2)) first += x;
  for (const x of values.subarray(2)) second += x;
  assert.equal((await engine.bulkReduce(windows, hm)).bands[0].sum, first + second);
}));

test('selection order, repeated contributions, spans, masks and classification counters', async () => withEngine(async engine => {
  const values = new Float64Array([10,-2,NaN,-999,Infinity,0,8]);
  const validity = new Uint8Array([1,1,1,1,1,1,0]);
  const windows = one(values, { validity, nodata: -999 });
  windows[0].selection = new Uint32Array([0,1,2,3,4,5,6,0]);
  const result = (await engine.bulkReduce(windows, hm)).bands[0];
  assert.deepEqual(result, { id:17, has_values:true, sum:20, min:0, max:10, mean:20/3,
    valid_count:3, excluded_mask:1, excluded_nodata:1, excluded_nonfinite:2, excluded_negative:1 });
  await assert.rejects(engine.bulkReduce(windows), /nonfinite|non-finite/i);
  windows[0].selection = { spans: new BigUint64Array([0n,2n,5n,1n]) };
  assert.equal((await engine.bulkReduce(windows, hm)).bands[0].sum, 10);
  windows[0].selection = new BigUint64Array([0n,0n]);
  assert.equal((await engine.bulkReduce(windows)).bands[0].sum, 20);
  const bits = await engine.bulkReduce(one(new Float32Array([1,2,3]), { validity:new Uint8Array([0b1010]),validityKind:'bits',validityOffset:1 }));
  assert.equal(bits.bands[0].sum, 4);
}));

test('aligned striding and empty selection have explicit output', async () => withEngine(async engine => {
  const result = await engine.bulkReduce(one(new Float64Array([99,1,99,2,99,3]), {byteOffset:8,byteStride:16,cellCount:3}));
  assert.equal(result.bands[0].sum, 6);
  const windows = one(new Float32Array([1,2])); windows[0].selection = new Uint32Array();
  assert.deepEqual((await engine.bulkReduce(windows, hm)).bands[0], {id:17,has_values:false,sum:0,min:null,max:null,mean:null,valid_count:0,excluded_mask:0,excluded_nodata:0,excluded_nonfinite:0,excluded_negative:0});
}));

test('malformed bounds, versions at binding policy, shared/resizable/detached inputs reject safely', async () => withEngine(async engine => {
  const values = new Float32Array([1,2,3]);
  for (const args of [
    [one(values), {policy:'unknown'}], [one(values), {reducers:['count']}], [one(values), {maxPayloadBytes:4}],
    [one(values), {reducers:['sum','sum']}],
    [one(values), {maxContributions:2}], [one(values,{byteOffset:1})], [one(values,{byteStride:0})],
    [one(values,{cellCount:4})], [one(values,{validity:new Uint8Array(1)})],
    [[{bands:[{values}],selection:new Uint32Array([3])}]],
    [[{bands:[{values,id:1},{values,id:1}]}]],
    [[{bands:[{values}]},{bands:[{values,id:5}]}]],
    [one(new Int32Array([1,2]))], [one(new Float32Array(new SharedArrayBuffer(16)))],
  ]) { await assert.rejects(engine.bulkReduce(...args)); assert.equal(bulkMemoryStats().owned_bytes,0); }
  const detached = new Float32Array([1]); structuredClone(detached.buffer,{transfer:[detached.buffer]});
  await assert.rejects(engine.bulkReduce(one(detached)));
  const resizable = new ArrayBuffer(16,{maxByteLength:32});
  if (resizable.resizable) await assert.rejects(engine.bulkReduce(one(new Float32Array(resizable))));
  assert.equal((await engine.bulkReduce(one(values))).bands[0].sum,6);
  const controller = new AbortController();
  const band = { get values() { controller.abort(); return values; } };
  await assert.rejects(engine.bulkReduce([{bands:[band]}], {signal:controller.signal}), error=>error.code==='ABORTED');
  assert.equal(bulkMemoryStats().owned_bytes,0);
}));

test('async snapshot prevents post-dispatch mutation and retains busy/cancel/close ownership', async () => withEngine(async engine => {
  const values = new Float32Array(1_000_000).fill(1);
  const windows = [{bands:Array.from({length:40},(_,id)=>({id,values:values.subarray(0,120000)}))}];
  const pending = engine.bulkReduce(windows, hm);
  values.fill(100);
  await assert.rejects(engine.stats(), error=>error.code==='BUSY');
  assert.equal((await pending).bands[0].sum,120000);
  const controller = new AbortController();
  const aborted = engine.bulkReduce(windows,{...hm,signal:controller.signal});
  controller.abort();
  await assert.rejects(aborted,error=>error.code==='ABORTED');
  assert.equal(bulkMemoryStats().owned_bytes,0);
  assert.equal((await engine.bulkReduce(one(new Float32Array([2])))).bands[0].sum,2);
  const closing = engine.bulkReduce(windows,hm);
  const rejected = assert.rejects(closing,error=>error.code==='ABORTED');
  await engine.close(); await rejected;
  assert.equal(bulkMemoryStats().owned_bytes,0);
}));

test('native bulk leaves Node timers running and never serializes pixels', async () => withEngine(async engine => {
  const windows = [{bands:Array.from({length:40},(_,id)=>({id,values:new Float32Array(120000).fill(1)}))}];
  let ticks = 0; const timer = setInterval(()=>ticks++,1);
  const original = JSON.stringify;
  JSON.stringify = () => { throw new Error('JSON must not be used by bulk'); };
  try { for(let i=0;i<4;i++) assert.equal((await engine.bulkReduce(windows,hm)).bands[0].sum,120000); }
  finally { JSON.stringify = original; clearInterval(timer); }
  assert.ok(ticks > 0);
}));
