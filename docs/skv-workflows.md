# Experimental SKV v0 workflows

SKV is an optional derived serving object. Compile a supported TIFF/COG once,
then query the `.skv` object independently of the original raster. Ordinary
TIFF/COG `infuse`, `carve`, `ward` and `cleave` remain available. SKV v0 is unstable;
retain the original source for rebuilding and do not treat this experiment as a
permanent interchange standard.

The compiler runs inside Skarve. It reads original typed samples and independent
mask bytes, preserves their declared interpretation and builds optional stored
summaries. Python and Node do not receive all source pixels or spawn a Python
conversion worker. No resampling, reprojection, overview selection or polygon
answer precomputation is part of conversion.

## Installed CLI

Generate the public deterministic fixtures with the test-only NumPy/rasterio
dependencies, then use the installed CLI:

```sh
python examples/generate_fixtures.py /tmp/skarve-skv-example-data
skarve compile /tmp/skarve-skv-example-data/original.tif \
  --output /tmp/skarve-skv-example-data/serving.skv \
  --chunk-edge 64 --band-group 4 --codec deflate
skarve verify-skv /tmp/skarve-skv-example-data/serving.skv
skarve carve /tmp/skarve-skv-example-data/serving.skv \
  /tmp/skarve-skv-example-data/polygon.json --crs EPSG:3857 \
  --metrics sum,support,mean,min,max
```

The output must not already exist. `--no-summaries` creates a raw-only object for
format/layout controls. `--bands 39,0` compiles an explicit derived subset in that
order; omission compiles every admitted band. A source specification can be
passed as `@source.json`, including bounded HTTP options and an explicit mapping.
`verify-skv` reads and validates all payloads; it is a deliberate full object read,
not a free metadata check.

Compilation options are `chunk_edge` (64, 128 or 256; default 256), `band_group`
(1–64; default 4), `codec` (`deflate` or `none`; default `deflate`),
`compression_level` (0–9; default 3), `summaries` (default true), `working_bytes`
(16–256 MiB; default 64 MiB), and `max_output_bytes` (up to 8 GiB; default 2 GiB).
The CLI uses matching hyphenated flags. These are admitted resource bounds; a
particular physical TIFF layout can still require more memory than requested.

## Python

```python
from skarve import Skarve

with Skarve() as sk:
    with sk.infuse("original.tif") as original:
        receipt = original.compile("serving.skv", chunk_edge=128, band_group=4)
    verification = sk.verify_skv("serving.skv")
    with sk.infuse("serving.skv") as source:
        result = source.carve(zone=polygon, metrics=["sum", "support", "mean"])
```

`await source.compile_async(output, **options)` and
`await sk.verify_skv_async(source)` use the same native operations off the Python
event loop. Cancellation drains native ownership before the awaiter returns.
Use `sk.infuse({"location": "serving.skv", "use_summaries": False})` to disable
embedded summaries without changing the source's values or backend policy.

## Node

```javascript
import Skarve from '@skarve/engine';

const sk = new Skarve();
try {
  const original = await sk.infuse('original.tif');
  const receipt = await original.compile('serving.skv', { chunk_edge: 128, band_group: 4 });
  await original.close();
  await sk.verifySkv('serving.skv');
  const source = await sk.infuse('serving.skv');
  const result = await source.carve({ zone: polygon, metrics: ['sum', 'support', 'mean'] });
  await source.close();
} finally { await sk.close(); }
```

Compilation and verification accept the ordinary `signal: AbortSignal` option.
The session retains one active native request and drains it on cancellation or
close. Cancellation is cooperative, with checks between bounded I/O/decode work;
it is not a hard deadline or process-isolated kill guarantee.

## Shared batch execution and optional backend

Use an SKV source specification in an ordinary `cleave` slice. All selected bands
remain part of one logical slice; native reads are internally divided into groups
according to the source capability and unchanged byte limits. SKV admits up to
64 bands per physical read; ordinary GDAL readers retain a twenty-band limit.
Resident reducers still consume groups of at most twenty references without
copying the wider read's values. Do not emulate a batch with a loop around `carve`.

```python
pages = list(sk.cleave({
    "zones": [{"id": "a", "version": "1", "geometry": polygon_a},
              {"id": "b", "version": "1", "geometry": polygon_b}],
    "slices": [{"id": "snapshot", "spec": {"location": "serving.skv"}}],
    "crs": "EPSG:3857",
    "metrics": ["sum", "support", "mean", "min", "max"],
}, max_rows=2))
assert pages[-1]["complete"]
```

Where the optional backend is installed and eligible, select
`backend="exactextract"` on `carve` or `cleave`. Its existing
`exactextract_fractional_v030` policy and provenance remain explicit. It consumes
Skarve-interpreted raw windows and does not inherit native summary shortcuts.
The default remains native strict. HM ordered folding and spherical policies are
separate contracts; planar stored summaries do not assert their equivalence.

For the explicit R12 `band_group=1` physical-order choice, see the
[band-major exactextract recipe](skv-band-major.md). It includes installed
single/batch calls and measured native regressions; existing defaults remain.

Runnable installed examples: [Python](../examples/python_skv.py) and
[Node](../examples/node_skv.mjs). The generated-only
[installed qualification](../tests/skv_installed.py) and its
[Node companion](../tests/skv_installed_node.mjs) exercise forty distinct bands,
all four polygon/band cases, masks, thin geometries, raw-only serving and a missing
original source pathname. They do not establish cloud or independent-hardware
performance. See the format specification and measured report for those boundaries.
