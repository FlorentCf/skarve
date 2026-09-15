# Typed selected-window values

Use `engine.bulk_reduce(windows, ...)` in Python or `await engine.bulkReduce(windows, ...)` in Node when the caller already owns Float32/Float64 values and an ordered selection. Ordinary source-owned polygon queries remain separate. The bulk result establishes no persistent raster/index identity or geometric selection provenance.

Each window has an ordered band list with numeric `id`, typed `values`, optional exact raw `nodata` and validity mask. Omitted selection visits every logical cell. Indices may be uint32/uint64 and may repeat; spans retain their stated order and count repeated contributions. All windows must preserve the same ordered band identities. Node spans are an interleaved `BigUint64Array` of start/count; Python uses a contiguous uint64 N-by-2 array. See the versioned C header for the native descriptor contract.

`strict_selected_v1` is the default: signed finite selected values, existing compensated accumulation and explicit rejection of valid nonfinite values. `hm_demographics_ordered_v1` is an explicit compatibility policy: mask, exact NoData, nonfinite and negative exclusions in that order; binary64 left-fold per band/window, followed by the ordered fold of window partials. It does not apply overview correction or display rounding. Different window partitioning may change ordered floating-point results, so callers must preserve that partition as part of the contract.

```python
import numpy as np
from skarve import Engine
with Engine() as engine:
    values = np.array([1.25, -2, 0, 3.75], dtype=np.float32)
    windows = [{'bands': [{'id': 100+b, 'values': values} for b in range(40)]}]
    assert engine.bulk_reduce(windows)['bands'][0]['sum'] == 3
    assert engine.bulk_reduce(windows, policy='hm_demographics_ordered_v1')['bands'][0]['sum'] == 5
```

Python/Node runnable examples cover 36 and 40 bands. The limit is 64 logical bulk bands; ordinary resident raster capacity remains 20. Further limits are 4,096 windows, 268,435,456 selected band contributions and 128 MiB per-call reservation including snapshot, control and output storage. Both bindings reserve at most 512 MiB active snapshot/control memory per process. Reduce these limits for your embedding; larger caller arrays and complete process memory are additional costs.

Bindings take one owned, dtype-preserving snapshot of each supplied view. No pixel JSON or universal full Float64 normalization is required. Python accepts native-endian contiguous NumPy Float32/Float64 views and rejects noncontiguous arrays. Node accepts fixed non-shared buffers and rejects detached/resizable inputs; aligned offsets/strides are explicit. Whole supplied views remain charged even for sparse selection. Snapshot copying is synchronous in Node; native execution uses asynchronous FFI. Abort/close retains ownership until the native call is drained.

Reducers are `sum`, `min`, `max` and `mean`; classification/count fields are always returned. No-support sum is zero and extrema/mean are null when requested. Repeated reducers, invalid identities, selection overflow, invalid masks, unsupported types and arithmetic overflow reject. Output is published only after the complete request succeeds. The raw C caller must still supply valid live allocations: descriptor bounds cannot prove an arbitrary pointer safe. Focused ASan evidence is not a whole-engine sanitizer/fuzzer claim.
