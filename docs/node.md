# Node and TypeScript

```js
import Skarve from '@skarve/engine';

const sk = new Skarve();
try {
  const source = await sk.infuse('example-data/original.tif');
  const result = await source.carve({zone:polygon, bands:[0], metrics:['sum','mean']});
  console.log(result.bands[0].fractional_sum);
} finally { await sk.close(); }
```

The installed package includes [TypeScript declarations](../bindings/node/index.d.ts).
The default export, `Skarve`, `RasterEngine` and `Engine` are the same class.
Polygons must already use the source CRS; an explicit `crs` asserts a match.

Native calls use asynchronous FFI, including optional C++ backend work. They do
not run a long geometry operation on Node's event loop. Await each request:
there is one active call per session, at most four live Node sessions, and no
unbounded request queue. `signal: abortController.signal` requests cooperative
cancellation. The returned promise rejects after native completion; `close()`
cancels and drains before freeing session/input owners. No hard kill latency is
implied for an upstream geometry phase.

Use `for await (const page of sk.cleave(job, {maxRows:128}))` for [batch](batch.md);
breaking the iterator closes its job. `source.ward(...)` creates [optional
preparation](preparation.md). The explicit [bulkReduce](TYPED_BUFFERS.md) method
keeps typed-buffer snapshots leased until native completion.

Run [node_source.mjs](../examples/node_source.mjs) for a complete installed
workflow. [Compatibility aliases](api.md) preserve existing callers.
