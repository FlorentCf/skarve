# Backends and calculation policies

Backend choice and numerical meaning are separate request fields. Native strict
is unchanged when neither is supplied. Optional exactextract is the pinned
upstream fractional-coverage implementation, not a center/all-touched method.
Finite-precision differences from native strict are compatibility findings.

| Request | Eligible calculation |
|---|---|
| No backend/policy fields | Native strict |
| `backend: native` | `native_grid_planar_fractional` |
| `backend: exactextract` with no policy field | `exactextract_fractional_v030` |
| `backend: exactextract`, explicit `numerical_policy: exactextract_rasterio_v030` | Unscaled typed Rasterio-compatible input contract |
| `backend: auto` with no policy field | Native strict only |
| `auto` with explicit `accepted_policies` | Installed, supported backends within those policies and the execution envelope |

Use either `numerical_policy` or `accepted_policies`, never both. A forced
exactextract call requiring `native_grid_planar_fractional` fails. Accepting
multiple policies permits finite-precision results to change when selection changes;
no bitwise equality or universal error bound is implied. Auto is conservative,
not a promise of the fastest possible execution.

```python
result = source.carve(
    zone=polygon, bands=[0], metrics=['sum','support','mean','min','max'],
    backend='exactextract', numerical_policy='exactextract_fractional_v030',
    execution_envelope='embedded_cooperative',
    backend_options={'strategy': 'raster-sequential',
                     'max_cells_in_memory': 262144,
                     'window_bytes': 64 * 1024**2,
                     'output_bytes': 16 * 1024**2,
                     'max_windows': 16384,
                     'decoded_bytes': 2 * 1024**3},
)
print(result['provenance'])
```

The same fields belong at the top level of a `cleave` job. Node uses identical
field names inside `carve({...})` and the job object. Retain returned backend,
version, calculation policy and source interpretation provenance with results.

Runnable single-and-batch examples use generated data from Getting started:

```sh
python examples/python_backend.py --fixture example-data/fixture.json --backend native
python examples/python_backend.py --fixture example-data/fixture.json --backend exactextract
node examples/node_backend.mjs example-data/fixture.json exactextract
```

The exactextract commands require a build containing the optional backend.
Without it, explicit selection fails clearly while native examples still work.

The legacy `exactextract_fractional_v030` shared-reader contract is `skarve_normalized_f64_v1`: Skarve applies the
declared source mask, raw NoData, scale and offset, then supplies interpreted
binary64 values to upstream exactextract. This is not automatically identical
to every upstream Rasterio adapter's Float32 scale/offset behavior. Compare
matching interpreted values when validating delegation.

The opt-in [`exactextract_rasterio_v030` policy](exactextract-rasterio.md) uses
original unscaled Float32/Float64 samples, their masks and bounds-derived
resolution. It preserves the legacy policy, native defaults and original stored
grid. Its narrower source eligibility is checked explicitly; unsupported inputs
fail without a legacy fallback.

The optional operation envelope is fractional sum, valid fractional support,
mean, min and max for supported polygon/multipart geometry and compatible
sources/bands. Native integer count and advanced statistics are not fabricated.
Request any supported subset with `metrics`, such as `['min']`. Only requested
output operations plus internal support are registered. Upstream can still
accumulate sums internally; an unrequested sum overflow does not invalidate a
finite minimum. An overflowing requested sum still fails explicitly.
Multiple sources in one delegated call must have exactly equal grids and CRS;
there is no implicit alignment, resampling or overview choice.
Batch admission is limited to 512 zones, 32 slices and 64 logical bands. Its
conservative full-grid contribution bound can reject large sparse jobs; see
[Batch](batch.md) for the bound, output staging and unsupported selectors.
Ordered selected-window sums and spherical population allocation remain separate
native-only policies. Preparation remains optional; an exactextract request
cannot silently claim it executed a native summary index.

The embedded backend is cooperatively cancellable. Window/read/progress checks
can stop work, but an in-progress upstream geometry phase may finish before the
call drains. Node abort and Python async cancellation keep native ownership
until completion. `process_isolated` is ineligible for this embedded path:
there is no hidden Python worker or promise of a hard kill deadline. Cell/window
budgets bound tracked work, not whole-process RSS. Apply an external process
limit when the deployment needs one.

An interrupted remote GDAL read can quarantine its reader with a persistent
transport error. After cancellation drains, close that reader and `infuse` the
original verified specification again; the session remains usable. See
[source recovery](sources-and-caching.md) and the bounded
[failure/cancellation observations](../benchmarks/beta2/FAILURE_CANCELLATION.md).

The pinned upstream feature implementation uses a shared GEOS context, so
embedded calls serialize across sessions. A waiting call still observes
cooperative cancellation. This backend makes no parallel throughput claim.

Unavailable dependencies, unsupported statistics/source combinations, policy
conflicts, changed sources, callback failure and exceeded budgets return errors.
No partial mixed-backend answer is reported as success. Build availability and
the measured automatic-selection disposition are specified by this candidate's
[installation](INSTALL.md) and [performance](performance.md) documentation.
