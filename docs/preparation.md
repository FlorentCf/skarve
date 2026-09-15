# Optional preparation

Ordinary source queries require no preparation. For a stable repeated workload,
`source.ward` can build a source-bound summary index:

```python
with sk.infuse('example-data/original.tif') as source:
    built = source.ward('new-index', boundary_source='original', tile_edge=16)
    with source.open_index('new-index/summary.rsi',
                           expected_build_id=built['build_id']) as index:
        result = index.measure(polygon, 'EPSG:3857', bands=[0])
```

Node uses `await source.ward(...)` and `await source.openIndex(...)`, followed by
the index's existing `measure` method. Summary-only preparation retains the
original source for boundary values and avoids copying the entire raster.
Eligible fully covered interior blocks can be answered without raw reads.

Charge build duration, summary bytes, registration, update/rebuild cost and any
auxiliary value representation separately from queries. Reuse with a changed
source or wrong build ID fails. A failed retained index handle must be closed
and reopened. Sources that change frequently and one-off jobs may never repay
preparation. [Benchmark evidence](performance.md) includes lifecycle costs.

Optional exactextract does not implicitly consume or impersonate the native
summary index. Backend/index combinations must be explicitly supported; an
ineligible combination errors instead of silently changing execution meaning.
