# Original typed source windows

`Source.readWindow` (Node), `Source.read_window` (Python), and
`Source::read_window` (Rust) expose original supported scalar samples with
independent mask bytes from a registered TIFF/COG or SKV source. They reuse its
selected existing overview, exposed band mapping, checksums, conditional HTTP
access, cancellation and source-generation checks.

This operation does not apply masks, NoData, scale/offset, interpolation,
resampling, negative-value filtering or polygon coverage. Applications choose
how to interpret the returned samples. For polygon statistics, use `carve` or
`cleave` with an explicit numerical policy.

## Read a window

The following Node example uses the generated file from the
[installation guide](INSTALL.md#try-generated-data):

```js
import Skarve from '@skarve/engine';
const engine = new Skarve();
try {
  const source = await engine.infuse('example-data/original.tif');
  try {
    const result = await source.readWindow({
      window: [0, 0, 1, 1], // x, y, width, height in the exposed grid
      bands: [0],           // distinct zero-based exposed band indices
      maxBytes: 16 * 1024 * 1024,
      workingBytes: 64 * 1024 * 1024,
    });
    const {values, mask, metadata} = result.bands[0];
    console.log(values[0], mask[0], metadata);
  } finally { await source.close(); }
} finally { await engine.close(); }
```

```python
from skarve import Skarve
with Skarve() as engine:
    with engine.infuse('example-data/original.tif') as source:
        result = source.read_window([0, 0, 1, 1], [0],
                                    max_bytes=16 << 20, working_bytes=64 << 20)
        samples = result['bands'][0]['values']  # typed memoryview
        mask = result['bands'][0]['mask']       # independent byte memoryview
        # read_window_async has cancellation-and-drain semantics.
```

Python source-window reads require neither NumPy nor rasterio. Node returns
Uint8/Int8/Uint16/Int16/Uint32/Int32/Float32/Float64 arrays as appropriate;
Python returns matching typed memoryviews. These bindings require a
little-endian host. Views share their result's backing allocation and remain
valid after the source or session closes.

Rust accepts caller-owned storage:
`source.read_window([0, 0, 1, 1], vec![0], &mut output, 64 << 20)?`.
See [the Rust guide](rust.md) for source creation and ownership.
The [C ABI header](../include/skarve_source_buffer.h) declares `re_read_window`:
JSON contains controls and descriptors, while pixels use caller-owned storage.
Free returned JSON with `re_free_string`.

## Values, descriptors and resource admission

Samples retain their original little-endian bits, including invalid payloads,
NaN payloads and signed zero. Masks preserve their independent byte values,
including nonbinary masks. Bands appear in requested order. Descriptors provide
`byteOffset`, `byteLength`, `maskOffset` and `maskLength`; values begin at aligned
offsets. `scaleBits`, `offsetBits` and optional `nodataBits` contain hexadecimal
f64 metadata bit patterns from GDAL's metadata interpretation. Missing NoData
is null. These metadata values are not applied to samples.

Registration and inspection include `rawMetadata` beside normalized `metadata`.
It describes scalar types, the selected source overview, and admission limits.
`maxReadBands` is the reader's raw group limit; `maxWindowBands` is the complete
output-window limit, at most 64. Node, Python and Rust can submit wider complete windows
that native execution partitions into admitted groups under one verification
boundary. Node and Python fall back to `maxReadBands` if `maxWindowBands` is absent.

Output capacity is at most 64 MiB and working memory at most 128 MiB. The
working reservation includes the entire caller buffer, the largest admitted
raw-group scratch bound and 16 MiB for controls, descriptors and diagnostics.
Preflight checks use each window's physical source footprint, not only its
sample count. Source retained capacity is also charged to session admission.
`reservedBytes` describes admission bounds, not measured RSS. Arrays retained
by application code after return require the application's own memory budget.
Use smaller windows or fewer bands when admission fails.

Only consume output after success. C and Rust caller storage may contain partial
bytes after failure or cancellation and must be discarded. Node/Python do not
return failed buffers. Ordinary reads retain pre/post generation verification.
One operation may run per session; there is no hidden queue. Async cancellation
waits for native work to finish before releasing storage or a handle; it is
cooperative cancellation, not process isolation.

## Reuse a source across queries

Node `source.metrics()` returns observational counters without source access;
it does not prove the current remote generation. `source.beginQuery()` renews
an HTTP source's original per-query request, download and transport logical-read
allowances without enlarging them. Counters remain cumulative and budget-base
fields identify each epoch. Registration and initial metadata consume the
initial allowance unless the application explicitly starts another epoch.
Cache capacity, identity and invalidation rules do not change. Local sources
verify their identity rather than receiving HTTP budgets.

SKV also has a separate **65,536 logical-read lifetime limit** in its reader.
Query renewal does not reset this limit. Reopen a source when a fresh reader
is needed; a failed or cancelled handle must be discarded. Expired signed URLs
also require reopening with a usable URL.

For a multi-window operation, Node provides `source.withVerifiedQuery`:

```js
const result = await source.withVerifiedQuery(async current => {
  const first = await current.readWindow({window: [0, 0, 1, 1], bands: [0]});
  const second = await current.readWindow({window: [1, 0, 1, 1], bands: [0]});
  return {first, second};
});
// Publish or consume the complete result only here, after final verification.
```

The callback's intermediate data is provisional: do not publish it or perform
irreversible work with it inside the callback. Uncached HTTP payload reads stay
conditional; a final physical generation check validates the complete operation
before the helper returns its result. Any failure closes that source. Queries
cannot nest, and budgets cannot renew inside an active verified query.
`renewBudget` defaults to true; pass `{renewBudget: false}` as the helper's
second argument to retain the current allowance.

The low-level JSON operations are `source_metrics`, `begin_source_query`,
`begin_verified_source_query` (optional `renew_budget`) and
`end_verified_source_query`, each with a `source` identifier. They are available
through the engine request interface; Python currently has no matching source
convenience methods. Successful finalization can return an HTTP identity receipt
for that exact source handle. Registration metadata and metrics alone are not
verification receipts, and a receipt does not cover other source dependencies.

## Bounded HTTP preparation

For HTTP TIFF/COG, `http.metadata_prefetch_bytes` optionally seeds the existing
range cache with a prefix of at most 512 KiB. The default is zero. The seed must
fit configured range, download and cache bounds, including cache-entry overhead.
All bytes count as physical I/O; the prefix can include payload as well as
metadata. It is not pinned and remains subject to normal LRU eviction.

`http.small_read_page_bytes` optionally serves TIFF reads of at most 4 KiB using
a containing power-of-two page of 4–64 KiB. The default is zero. Straddling reads
or insufficient remaining byte budget use exact reads. Larger reads are
unchanged. This can read extra bytes: `small_read_page_fetches` and
`small_read_page_overread_bytes` report that work. Independent revalidation still
contacts the provider. Both options reject local sources and SKV, preserve
conditional requests, and do not increase traffic or cache limits themselves.

Remote TIFF cache capacity can explicitly be configured up to 16 MiB; other
source configurations retain the 8 MiB maximum. Actual retained capacity is
included in session admission. Required-range preparation can optionally use
`SKARVE_HTTP_CONCURRENCY=2` for two bounded workers. Its additional buffers and
worker stacks are charged to admission. Connection pooling is enabled by default; set `SKARVE_HTTP_SHARED_POOL=0`
for independent clients. Parallel preparation defaults to one request. Pooling
is restricted to compatible origins; authorization remains request-local.
Source identities, traffic limits and per-source caches remain separate. The
registry retains at most eight pools, but active readers can retain evicted
clients: this is not a total socket, TLS-buffer or process RSS cap. Cancellation
of blocking HTTP reads remains timeout-dependent, including while joining workers.
Neither preparation nor connection reuse establishes a general speedup; physical
traffic and source layout determine the tradeoff.

## SKV capacity and correctness checks

SKV v0 admits 131,072 independent typed leaf records or 16,777,216
ordered grouped typed leaf records without changing its byte layout. Larger
grouped objects validate contiguous payload order through neighboring
descriptors and retain a fixed-capacity overlap guard. Source retained
admission remains 16 MiB, directory cache 64 pages, and object size at most
128 GiB. These are finite capacity limits, not a
promise that every source fits or that conversion improves size or speed.
Compilation checks actual source-window memory bounds before producing output.
Full local verification has a finite allowance derived from validated record
and page counts, restoring the serving read limit afterward, including on
failure. Remote verification keeps normal transport and serving bounds.

Generated-fixture correctness checks include `cargo test --test source_buffer`
and `python tests/source_buffer.py -v`. Generate a new 40-band fixture with
`python tests/source_buffer_fixture.py /tmp/source-window-fixture.tif`, then run
`node tests/source_buffer_node.mjs /tmp/source-window-fixture.tif` and
`node tests/source_buffer_node_http.mjs /tmp/source-window-fixture.tif`.
The HTTP tests use controlled loopback servers. Fixture generation needs the
NumPy/rasterio test dependencies from the installation guide. For a development
library, set `SKARVE_LIBRARY` explicitly; ordinary installed use follows the
package's library resolution. These checks establish correctness contracts,
not performance evidence.
