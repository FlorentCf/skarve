# Skarve for Node.js

Linux x86-64 experimental build `0.1.1-alpha.0`. Install the matching private `.tgz`
with `npm install ./skarve-engine-0.1.1-alpha.0.tgz`. The native core and Koffi
are included; Ubuntu 24.04 with GDAL 3.8.4 is the qualified runtime.

See the repository [installation guide](https://github.com/FlorentCf/skarve/blob/main/docs/INSTALL.md)
and [source-owned workflows](https://github.com/FlorentCf/skarve/blob/main/docs/WORKFLOWS.md).
Create a session with `new Skarve()`, register a source with `await sk.infuse(path)`,
then call `await source.carve({zone: polygon, bands: [0], metrics: ['sum','mean']})`.
Use `source.ward()` for optional preparation and `sk.cleave()` for shared batch
pages. The explicit `bulkReduce` interface accepts typed selected buffers.
Existing `RasterEngine`, `openSource`, `measure`, `prepare` and `batchPages`
names remain available. Optional exactextract uses the same query interface with
its own declared policy; native strict stays the default.
Experimental SKV v0 conversion is available with `await source.compile('data.skv')`;
the completed file can serve through `infuse`, `carve` and `cleave` without the original.
The format is experimental and carries no permanent compatibility promise.
Apache-2.0 applies to Skarve; bundled dependencies retain their own licenses and notices.
