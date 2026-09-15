# API reference

Skarve's branded methods are thin names over the owned session, reader and real
batch interfaces. They do not copy raster values or create a second backend API.

| Public | Compatibility | Purpose |
|---|---|---|
| `Skarve()` | Python `Engine()`; Node `RasterEngine` / `Engine` | Start a session |
| `infuse(source)` | `open_source()` / `openSource()` | Register a reader |
| `source.carve(zone, …)` | `source.measure(geometry, …)` | Polygon statistics |
| `source.ward(index, …)` | `source.prepare(index, …)` | Optional preparation |
| `source.compile(path, …)` | same name | Experimental lossless SKV snapshot |
| `verify_skv(spec)` / `verifySkv(spec)` | same operation | Explicit complete SKV verification |
| `cleave(job, …)` | `batch_pages()` / `batchPages()` | Bounded shared batch pages |
| `inspect()` | `inspect()` | Source/index metadata |
| `close()` | `close()` | Release owned resources |
| `bulk_reduce()` / `bulkReduce()` | unchanged | Advanced selected typed buffers |

Python `carve(zone, *, bands=None, metrics=None, backend=None, crs=None, **options)`
and Node `carve({zone, bands, metrics, backend, crs, ...options})` pass `metrics`
to native `statistics`. Supplying both spellings is an error. Band indices are
zero-based positions in the registered source mapping; `infuse({location: path,
bands: [2, 0]})` makes query band `0` refer to original band `2`.

Omitting `crs` asserts that the polygon is already in the registered source CRS.
It does not transform coordinates or infer a polygon's CRS. An explicit mismatch
fails. `infuse` generates distinct session IDs when `id` is omitted. Existing
`open_source` / `openSource` defaults retain their compatibility behavior.

`carve` accepts [backend policy and execution requirements](backends.md).
Returned native field names remain explicit: `fractional_sum`,
`covered_cell_equivalents`, `coverage_weighted_mean`, `min`, `max`.
Fractional support is not an integer count. Fields unsupported by the selected
backend are rejected rather than invented.

`cleave(job, page_options)` forwards one job to the ordinary batch interface. Its
optional top-level `metrics` maps to `job.options.statistics`; both at once are
an error. The job contains `zones`, `slices`, `crs`, backend requirements and
resource budgets. Native jobs share source scans; optional exactextract jobs use
one bounded upstream processor with typed staging. It is not a loop around
`carve`. See [Batch](batch.md).

All handles belong to their session. Do not reassign or close their IDs through
the low-level request API while wrappers remain live. Node requests run in the
bounded asynchronous FFI pool, with one active call per session. Python source
operations are synchronous; `await source.carve_async(...)` or `call_async(...)`
runs off the event loop and drains cancellation before returning.

SKV is an optional self-contained source representation. `infuse`, `carve` and
`cleave` use the same contracts for its raw path, stored summaries and eligible
optional backend. Conversion is explicit and accepts no query polygons. See the
[installed SKV workflows and limits](skv.md) and [format specification](skv-format-v0.md).
