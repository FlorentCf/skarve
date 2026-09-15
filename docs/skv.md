# Compile and serve an SKV snapshot

SKV v0 is an **unstable, optional serving format** in the isolated
0.1.1-alpha.1 experiment. The frozen0.1.0-beta.2 product and artifacts remain
separate. An SKV stores typed source samples, original mask bytes, interpretation
metadata and optional aggregate summaries in one file. Querying it needs neither
the original TIFF/COG nor a sidecar. The ordinary direct source path remains valid.

Use the experimental artifacts and their own `SHA256SUMS` and `manifest.json`;
see [installation](INSTALL.md). No public registry or cloud destination is implied.
The supported binary runtime remains Ubuntu24.04/Linux x86-64/GDAL3.8.4.

Linux builds decode existing zlib packets through system libdeflate 1.19 with a
per-call allocator capped at64 KiB, inside the existing8 MiB reader scratch
allowance. `access_layout.deflate_decoder` reports the implementation and
`diagnostics.metrics.libdeflate_context_peak_bytes` reports its largest payload
decoder allocation. Other platforms retain miniz; the Linux library requires
the runtime specified in [installation](INSTALL.md). Compression, checksums,
lossless interpretation and stored bytes remain unchanged. Cancellation is
checked before and after each bounded native decode (at most4 MiB output),
so an active decode completes before cancellation is observed.

Known remote SKV sources combine registration and their16 KiB bootstrap into one
validated range request. Use an explicit `format: "skv"` when the URL has a
non-SKV suffix. Query-boundary generation checks still contact the provider:
conditional HEAD for URLs without a query string, or the existing uncached
conditional byte0 GET probe for query-bearing URLs. Subsequent ranges remain
conditional. Bootstrap success alone does not validate a provider's presigned-URL
policy; the full query must support these ranges and generation probes. Registration requires a16 KiB range and
download allowance even when the response cache is disabled. See the exact
[format and transport contract](skv-format-v0.md).

## Installed conversion and queries

These commands use the mathematical fixtures from the source archive. Fixture
generation uses NumPy/rasterio as test dependencies; Skarve itself owns conversion,
source access, decoding, geometry and aggregation.

```sh
python examples/generate_fixtures.py example-data
skarve compile example-data/original.tif --output example-data/snapshot.skv \
  --chunk-edge 64 --band-group 4 --codec deflate
skarve verify-skv example-data/snapshot.skv
skarve infuse example-data/snapshot.skv
skarve carve example-data/snapshot.skv example-data/polygon.json \
  --crs EPSG:3857 --bands 0 --metrics sum,support,mean,min,max
```

The output must be new. Compilation performs complete typed verification before
atomic no-replace finalization. `verify-skv` is an explicit complete-object check;
ordinary serving checks the pinned generation and the pages/payloads it reads.
Full-file verification is not hidden in each cold query.

Run the installed Python and Node examples, each using a fresh output filename:

```sh
python examples/python_skv.py --fixture example-data/fixture.json \
  --output example-data/python.skv
node examples/node_skv.mjs example-data/fixture.json example-data/node.skv
```

Each example closes the original reader, registers the SKV, consumes a complete
`carve` result and a bounded `cleave` batch. Node calls the native engine directly;
it requires no Python worker. All band indices refer to the registered mapping.

The underlying calls are `source.compile(path, **options)` in Python and
`await source.compile(path, options)` in Node. Explicit complete verification is
`engine.verify_skv(path)` / `await engine.verifySkv(path)`. The
[API](api.md), [batch schema](batch.md) and [cancellation contract](backends.md)
apply to SKV as to other readers.

## Optional encoding and summaries

`chunk_edge` accepts64/128/256, `band_group`1..64, `codec` `none`/`deflate`, and
`compression_level`0..9. `predictor` is `none` by default; the optional
`byte_delta_v1` preserves every sample bit while applying byte-plane horizontal
differencing before compression. It changes storage representation, not values.
Use `--predictor byte_delta_v1` in the CLI, or pass the same option to `compile`.
Performance and storage depend on the dataset; this option is measured separately.

`payload_layout="band"` remains the default: each band's tile has its own
compressed payload. The optional `row_group_v1` layout places rows from a
bounded group of bands in one payload, retaining the selected predictor and
the exact original masks. It is intended for testing repeated multiband reads.
Use `--payload-layout row_group_v1` in the CLI or the same `payload_layout`
option in Python and Node:

```sh
skarve compile example-data/original.tif --output example-data/grouped.skv \
  --chunk-edge 128 --band-group 40 --predictor byte_delta_v1 \
  --payload-layout row_group_v1
```

`band_group` is a maximum. Groups split at scalar-width changes and the bounded
packet size. A 128×128 tile of 40 Float32 bands fits one group; some other
type/tile combinations split. Selecting one band still fetches and decodes its
whole group. This can substantially increase single-band traffic, including
exactextract's per-band reads. Grouping does not change arithmetic, band order,
validity, source verification, query memory limits or the default backend.
Treat it as an explicit experimental workload choice until the measured serving
profile is qualified. Old independent-band SKV files remain supported; older
readers reject the grouped format's required feature flag.

Summaries are enabled by default. `--no-summaries` or `summaries=False`/`false`
builds the raw control. To query an existing summarized file through the raw path,
register `{"location":"example-data/snapshot.skv","use_summaries":false}`.
Eligible native queries can avoid fully covered interior payloads; numerical
policies requiring raw access retain it. An unrepresentable aggregate disables
summaries for that object with an explicit reason, while preserving raw samples.

The optional exactextract build can query the same SKV through
`backend="exactextract"` and `numerical_policy="exactextract_fractional_v030"`.
Its processor uses raw normalized values, with its own declared policy; it does
not silently consume native summaries. Feature/raster strategy, memory and
cooperative cancellation are governed by the [backend contract](backends.md).
Native strict remains the ordinary default.

The explicit [band-major recipe](skv-band-major.md) documents R12's measured
`band_group=1` option for raster-sequential exactextract, including CLI, Python
and Node calls, native tradeoffs and the full cost of retaining two layouts.
It does not replace the native or HM recipe.

The explicit [ordered source-selection interface](ordered-source.md) supports
HM's existing raw ordered policy without summaries. It is distinct from the
fractional polygon API. The same guide describes lossless compilation of an
explicit existing TIFF overview; it is never selected or generated implicitly.

## HTTP and resource limits

SKV uses the existing bounded conditional range reader. A remote object needs a
stable strong ETag and correct Range/If-Match behavior. For a generated small file,
the included loopback helper permits a credential-free serving check:

```python
import json
import sys
sys.path.insert(0, "examples")
from range_server import served
from skarve import Skarve

with open("example-data/polygon.json") as stream:
    polygon = json.load(stream)
with served("example-data/snapshot.skv") as server, Skarve() as sk:
    # The helper uses a fixed TIFF-shaped URL; explicit format identifies SKV.
    with sk.infuse({"location": server.url, "format": "skv",
                    "http": {"allow_http": True}}) as source:
        answer = source.carve(zone=polygon, metrics=["sum", "mean"])
    print(answer["bands"])
    print(server.snapshot())
```

This is a local HTTP check, not an R2/WAN result. No upload is implied. Remote
failures invalidate that reader: close it and register the current object again.
Never accept a partial scientific result after a failed operation.

Compiler `working_bytes` is16..256MiB (default64MiB, actual minimum bound32MiB).
Some source layouts require an explicit128MiB allowance. Reader, query, HTTP
request/byte/cache and batch output limits remain separate and enforced. An SKV
v0 object is limited to8GiB,64 stored bands and48,000 typed leaf chunks. Metadata
has additional limits. Complex/64-bit integer samples, rotated/reprojected grids
and arbitrary TIFF tag reconstruction are outside this lossless contract.

See the [binary specification](skv-format-v0.md) for supported types, raw-bit and
mask semantics, corruption checks, version behavior and complete cost accounting.
Rebuild the snapshot explicitly when the source changes; SKV is not an in-place
mutable raster database. Conversion and retained original-plus-SKV storage are
preparation costs and belong beside cold-query timings.
