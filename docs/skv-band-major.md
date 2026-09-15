# Optional band-major SKV for raster-sequential exactextract

The experimental R12 recipe uses existing compiler options to put independently
compressed tiles in band-major order. It is an explicit option for workloads
using the installed exactextract backend's `raster-sequential` strategy. It does
not replace the existing generic SKV recipe, native strict defaults, or the
separately qualified HM ordered-selection recipe. Automatic backend selection
does not imply automatic format or layout selection.

## What changes

Compile with `chunk_edge=128`, `payload_layout="band"`, `band_group=1`,
`predictor="byte_delta_v1"`, `codec="deflate"`, `compression_level=3`, and
summaries enabled. Omit a source band selection to retain all admitted bands.

For independent payloads, `band_group` controls physical order. A group spanning
all bands stores each spatial tile's band payloads together. A group of one
stores all spatial tiles for band 0, then all for band 1, and so on. Directory
record IDs and exposed band order stay the same. Adjacent same-band tiles can
then share a range read when the query needs them and the request cap permits.
This is **not** `row_group_v1`: no multi-band compressed packet is created, and
selecting one band does not require decoding the other bands.

The R12 conversion checks found identical per-record encoded payloads, typed
samples, masks, interpretation, logical digests and summary states for the old
and band-major objects. Offsets, file provenance and file hashes differ. This
is a layout choice, not a new algorithm, numerical policy or source contract.
The compiler defaults remain unchanged, including `band_group=4` and
`predictor="none"`.

## Installed calls

Use an experimental build containing the optional exactextract backend; check
`skarve backends`. The following examples use only the public mathematical
fixtures, need no HM code or credentials, and demonstrate API usage rather than
reproduce the larger R12 performance fixtures. Every compile output must be new.

### CLI

```sh
python examples/generate_fixtures.py example-data
skarve compile example-data/original.tif --output example-data/band-major-cli.skv \
  --chunk-edge 128 --band-group 1 --payload-layout band \
  --predictor byte_delta_v1 --codec deflate --compression-level 3 \
  --working-bytes 134217728
skarve verify-skv example-data/band-major-cli.skv
skarve carve example-data/band-major-cli.skv example-data/polygon.json \
  --crs EPSG:3857 --metrics sum,support,mean,min,max \
  --backend exactextract --numerical-policy exactextract_fractional_v030 \
  --backend-options '{"strategy":"raster-sequential","max_cells_in_memory":262144,"window_bytes":67108864}'
```

`verify-skv` is an explicit complete-object read. It is useful for validation and
is not a free step hidden in each cold query. To execute a batch with the CLI,
save a job using the `zones`, `slices`, `crs`, `metrics`, `backend`,
`numerical_policy` and `backend_options` fields shown below, then run
`skarve cleave job.json --max-rows 2`. Backend fields belong inside the job JSON.

### Python

```python
import json
from pathlib import Path
from skarve import Skarve

root = Path("example-data")
fixture = json.loads((root / "fixture.json").read_text())
output = root / "band-major-python.skv"
metrics = ["sum", "support", "mean", "min", "max"]
backend = {
    "backend": "exactextract",
    "numerical_policy": "exactextract_fractional_v030",
    "backend_options": {"strategy": "raster-sequential",
                        "max_cells_in_memory": 262144, "window_bytes": 67108864},
}
with Skarve() as sk:
    with sk.infuse(root / fixture["sources"]["original"]["file"]) as original:
        receipt = original.compile(
            output, chunk_edge=128, band_group=1, payload_layout="band",
            predictor="byte_delta_v1", codec="deflate", compression_level=3,
            summaries=True, working_bytes=134217728,
        )
    with sk.infuse(output) as source:
        single = source.carve(zone=fixture["geometries"]["small"],
                              crs=fixture["crs"], metrics=metrics, **backend)
        print(single["provenance"])
    rows, complete = 0, False
    for page in sk.cleave({
        "zones": [{"id": name, "version": "1", "geometry": fixture["geometries"][name]}
                  for name in ("small", "overlap")],
        "slices": [{"id": "snapshot", "spec": {"location": str(output), "format": "skv"}}],
        "crs": fixture["crs"], "metrics": metrics, **backend,
    }, max_rows=2):
        rows += len(page["rows"])
        complete = page["complete"]
    assert complete and rows == 2
```

### Node

```javascript
import Skarve from '@skarve/engine';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = 'example-data';
const fixture = JSON.parse(readFileSync(join(root, 'fixture.json'), 'utf8'));
const output = join(root, 'band-major-node.skv');
const metrics = ['sum', 'support', 'mean', 'min', 'max'];
const backend = {
  backend: 'exactextract', numerical_policy: 'exactextract_fractional_v030',
  backend_options: { strategy: 'raster-sequential', max_cells_in_memory: 262144,
                     window_bytes: 67108864 },
};
const sk = new Skarve();
try {
  const original = await sk.infuse(join(root, fixture.sources.original.file));
  try {
    await original.compile(output, {
      chunk_edge: 128, band_group: 1, payload_layout: 'band',
      predictor: 'byte_delta_v1', codec: 'deflate', compression_level: 3,
      summaries: true, working_bytes: 134217728,
    });
  } finally { await original.close(); }
  const source = await sk.infuse(output);
  try {
    const single = await source.carve({ zone: fixture.geometries.small,
      crs: fixture.crs, metrics, ...backend });
    console.log(single.provenance);
  } finally { await source.close(); }
  let rows = 0, complete = false;
  for await (const page of sk.cleave({
    zones: ['small', 'overlap'].map(id => ({ id, version: '1', geometry: fixture.geometries[id] })),
    slices: [{ id: 'snapshot', spec: { location: output, format: 'skv' } }],
    crs: fixture.crs, metrics, ...backend,
  }, { maxRows: 2 })) {
    rows += page.rows.length;
    complete = page.complete;
  }
  if (!complete || rows !== 2) throw new Error('Incomplete batch');
} finally { await sk.close(); }
```

Each batch is one ordinary shared `cleave` job; it is not a loop over the
single-polygon API. Python uses `max_rows`; Node uses `maxRows`. The optional
backend stages its bounded output and does not inherit native checkpoints.

## Eligibility and numerical meaning

This recipe does not make an otherwise unsupported query eligible. Exactextract
must be installed, source/geometry/grid requirements must hold, and the selected
statistics and configured resource limits must be supported. The documented
operation set is fractional sum, support, mean, min and max; multiple delegated
sources must have equal grids/CRS. A forced unsupported request fails explicitly.

The explicit `exactextract_fractional_v030` policy uses Skarve's
`skarve_normalized_f64_v1` interpretation: original mask and raw NoData, then
scale/offset and finite-value checks. It uses raw normalized values, not native
compensated summaries. It is neither HM's ordered sum nor a spherical population
calculation. Native strict and exactextract may differ numerically; preserve
returned backend/version/policy provenance. The [backend contract](backends.md)
also covers cooperative cancellation, output staging and resource bounds.

## Measured disposition and full storage cost

R12 is an explicit optional-backend layout choice, not a generic promotion.
The frozen comparison includes generated 36-band, real 36-band crop and generated
40-band data, all four polygon/band cases, native raw/summary controls, competent
TIFF/COG controls and indexed controls. Local and controlled HTTP results do not
establish WAN performance or a real 40-band dataset result. Two or three repeats
per cell do not establish p95 or statistical significance.

All 912 frozen operations completed and passed their numerical checks. For
exactextract, band-major SKV beat the preselected eligible same-bridge TIFF/COG
control in **28/28 median dataset/case/access cells and 84/84 paired lifecycles**.
Against the old SKV layout, it won 25/28 medians and 72/84 pairs, retaining three
local cell losses. These comparisons use the same explicit exactextract policy;
they do not credit switching from native to exactextract.

Native results reject generic promotion: band-major caused **23 material
same-mode regression cells** against old SKV, comprising 14 of 44 summary cells
and 9 of 28 raw cells. A material regression means the new median exceeds
`max(old median * 1.05, old median + 2 ms)`; these are performance losses, not
engine failures. A native boundary read can need all bands of one tile, whose
payloads group1 separates across the file. That can increase requests even when
transferred bytes are unchanged. Existing native/HM choices therefore remain.

The independent audit found exact old/new output agreement for all 284 paired
same-path results (200 native, 84 optional). Native comparison against TIFF/COG
retained the existing `1e-8 + 1e-10 * abs(reference)` numerical tolerance; the
optional same-bridge comparison used zero tolerance. This bounded evidence
supports an explicit layout option, not universal superiority. The private R12
evidence pack retains the frozen tasks, all raw records and audited losses.

For the three verified recipe pairs, old and band-major objects have the same
file size. Retaining both nevertheless stores two complete independent objects:

| Dataset | One SKV, bytes | Both layouts, bytes |
|---|---:|---:|
| Generated 36 bands | 5,770,145 | 11,540,290 |
| Real 36-band crop | 28,933,939 | 57,867,878 |
| Generated 40 bands | 24,731,327 | 49,462,654 |

These totals exclude retained original TIFF/COG sources and any other indexes.
They are not a promise that every future build has identical file size. A second
layout requires a separately charged full conversion/verification, temporary
build space and a complete rebuild when source values, masks or interpretation
change. There is no implicit deduplication or in-place rearrangement. Select an
explicit object according to the measured workload; do not silently switch
numerical policies to claim a format win.
