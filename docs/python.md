# Python

```python
from skarve import Skarve

with Skarve() as sk:
    with sk.infuse('example-data/original.tif') as source:
        result = source.carve(zone=polygon, bands=[0], metrics=['sum','mean'])
        print(result['bands'][0]['fractional_sum'])
```

The polygon must already use the source CRS. Provide `crs='EPSG:3857'` when you
want an explicit assertion; the omitted value means the inspected source CRS.
`inspect()` exposes the registered mapping and source identity. Retain the
session/source for subsequent polygons; closing the source releases its reader.

Synchronous methods release the GIL while native work runs. In an async service,
use `await source.carve_async(zone=..., ...)`. Only one active request is allowed
per session. Keep geometry/control objects unchanged until their async request
completes. Cancellation is cooperative and drains even if the task is cancelled
again while native code is finishing. Cancel and await an active request before
closing the session. Use bounded separate sessions for application concurrency.

`sk.cleave(job, max_rows=128)` streams [shared batch pages](batch.md).
`source.ward(index, boundary_source='original')` builds [optional preparation](preparation.md).
Advanced selected arrays use [bulk_reduce](TYPED_BUFFERS.md).

Run [python_source.py](../examples/python_source.py) for the complete installed
workflow. [API aliases](api.md) preserve `Engine`, `open_source`, `measure`,
`prepare` and `batch_pages`; `raster_engine_lab` remains an import compatibility
module. New code should import `skarve`.
