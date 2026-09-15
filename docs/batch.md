# Many polygons and raster slices

`cleave` submits one real shared batch job and retrieves bounded pages. Native
execution reuses geometry across compatible grids and scans source windows for
multiple zones. It does not call the single-polygon API once per row.

```python
job = {
    'zones': [{'id': 'area', 'version': '1', 'geometry': polygon}],
    'slices': [
        {'id': 'date-a', 'spec': {'location': 'example-data/original.tif'}, 'bands': [0]},
        {'id': 'date-b', 'spec': {'location': 'example-data/date-b.tif'}, 'bands': [0]},
    ],
    'crs': 'EPSG:3857',
    'metrics': ['sum','support','mean','min','max'],
    'backend': 'native',
    'tile_edge': 32,
    'budget': {'working_bytes': 96*1024**2, 'geometry_bytes': 8*1024**2,
               'tile_bytes': 8*1024**2, 'output_bytes': 1024**2,
               'max_windows': 64, 'decoded_bytes': 8*1024**2,
               'max_contributions': 1024**2, 'workers': 1},
}
for page in sk.cleave(job, max_rows=2):
    for row in page['rows']:
        print(row['zone_id'], row['slice_id'], row['bands'])
```

Node passes the same job to `sk.cleave(job, {maxRows:2})` in an async iteration.
The low-level `batch_pages` / `batchPages` compatibility forms take
`options: {statistics: [...]}` instead of the top-level `metrics` shorthand.

Every result preserves zone/version/slice identity and band ordering. Result
pages are backpressured; do not collect a complete zone-by-time array unless your
application budgets for it. Persist a supplied checkpoint only after consuming
its rows durably. Close Python's iterator with `pages.close()` on early exit;
Node's `break` performs generator cleanup.

Backend selection/policy/envelope fields belong at the top of the job. Optional
exactextract uses natural upstream feature/raster sequential execution over the
accepted source interpretation and the same row identity contract. It supports
the five common statistics; it cannot manufacture native integer count. See
[Backends](backends.md) for eligibility and cancellation limits.

The embedded exactextract job stages its entire finite typed result before
presenting pages. Its initial envelope is at most 512 zones, 32 slices and 64
logical bands, subject to tighter byte/read budgets. `max_rows` bounds a returned
page, not the upstream job's staging. Resume checkpoints, derived expressions,
derived masks and native indexes are unsupported for this backend; the native
batch engine retains its existing support.

For exactextract, `budget.max_contributions` admits a conservative upper bound:
full-grid cells × selected logical bands × zones, summed across source slices.
Its default is 1,000,000,000. A large sparse job can fail admission even when its
actual intersections would be much smaller. This does not mean the complete
source is copied. Geometry structure, reader residency, callback windows,
complete typed output and returned pages have additional tracked bounds;
upstream chunk sizes do not impose a whole-process RSS limit.

Omit native-only `schedule`, `geometry_layout`, `window_policy` and `tile_edge`
selectors on an exactextract job. Explicit unsupported selectors are rejected,
not silently interpreted as an upstream execution strategy. Use the backend's
`backend_options.strategy` for its supported sequential strategies.

Numeric output is a distinct compact schema with a descriptor. Native numeric
output requires its six basic statistics; optional backend output must retain
its own declared schema. Do not interpret one as the other or drop provenance
when persisting numeric columns. [Performance](performance.md) charges result
consumption and complete job lifecycles separately from retained query timings.
Exactextract pages repeat the same completed upstream job metrics; do not add
those counters/timings once per page. They describe shared work, not fresh work
performed independently for every output page.
