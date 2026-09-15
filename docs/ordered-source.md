# Ordered selections over a source

The experimental `sum_selected` / `sumSelected` interface reads original typed
samples through Skarve's source reader and reduces explicit ordered selections.
It supports the existing `hm_demographics_ordered_v1` policy with Float32 or
Float64 sources. Native fractional `carve` and `cleave` defaults are unchanged.
This interface does not infer a geometry mask, choose a source tier/year, simplify
a polygon, choose precision, or assemble application demographic buckets.

Use it when those decisions have already produced an exact center mask and an
ordered sequence of logical source windows. Every window is `[x,y,width,height]`
in the registered source's grid. Its selection is exactly one of `indexes` or
`runs`. Indexes must be strictly ascending local row-major indexes. Runs are
`[start,endExclusive]` pairs, sorted, disjoint and in bounds; they encode the same
index sequence compactly. Invalid order is rejected, never repaired by sorting.

```python
from skarve import Skarve

request = {
    "bands": [0],
    "polygons": [{"id": "zone", "windows": [
        {"window": [0, 0, 4, 2], "runs": [[0, 3], [4, 8]]}
    ]}]
}
with Skarve() as sk:
    with sk.infuse("example-data/original.tif") as source:
        answer = source.sum_selected(request,
            numerical_policy="hm_demographics_ordered_v1")
    print(answer["rows"])
```

The Node equivalent is `await source.sumSelected(request,
{numerical_policy:'hm_demographics_ordered_v1', signal})`. It uses the existing
native session, without a Python worker. Python also provides
`await source.sum_selected_async(...)`, with the ordinary drain-on-cancellation
contract. Save the request as JSON for the CLI:

```sh
skarve sum-selected example-data/original.tif selections.json \
  --numerical-policy hm_demographics_ordered_v1
```

The policy is mandatory. Fractional and exactextract policies cannot be passed
to this operation, and this policy cannot be passed to fractional queries.

For an owner-verified pair of equivalent serving representations, the engine's
one-shot `sum_selected(profile, request, ...)` / `sumSelected(profile, request,
...)` operation applies a measured eligibility rule to the actual request and
opens only the selected source. See [serving profiles](serving-profiles.md) for
the identity, provisioning, fallback and resource contract.

## Numerical and source contract

The reducer reads raw Float32/Float64 values without GDAL scale/offset and ignores
the separate GDAL mask. It excludes nonfinite values first, then values equal to
finite NoData, then negative values. Zero is valid unless it equals NoData.
Omitting `nodata` uses each selected band's source metadata; an explicit vector
provides one finite value or `null` per selected band. `null` means no NoData.
Source samples and their original masks remain lossless in SKV; this query policy
explicitly determines which metadata affects the answer.

Each band uses an ordinary binary64 left fold in the supplied index order inside
each logical window, followed by an ordinary ordered fold of those window
partials. Physical read chunks never become logical windows. The output contains
raw final sums and exclusion counts. Apply an application overview scale only
after the final window fold, then perform that application's bucket assembly and
rounding. Compensated summaries and the optional exactextract backend are
ineligible for this operation. Overflow, cancellation, changed source generation
and exhausted resource budgets return an error rather than a partial success.

`budget.working_bytes` defaults to64MiB and includes the owned plan, output,
partials and physical read scratch. `planning_bytes` defaults to16MiB. The source
reader's retained reservation and the native request-control reservation are
additional and remain included in session admission. The output reports actual
read calls/bytes and phase timings. Small requests should use runs where they
represent the selection compactly; raster values never cross the JSON bridge.
`budget.read_materialized_bytes` defaults to512MiB and bounds the cumulative
typed sample and mask bytes returned by physical reads, including overread.
This is distinct from compressed transfer and GDAL-internal decoding work.

## Explicit existing source views

For a supported TIFF/COG, `sk.infuse({"location":"data.tif", "overview":0})`
opens its first existing reduced-resolution view. Zero is a zero-based GDAL
overview index, **not a universal TIFF image/IFD number**. Validate the expected
grid and band count against the application's selected image. Omission always
opens native resolution. No new overview, resampling or query-size selection is
performed. Local explicit views currently require internal TIFF overviews;
external overview sidecars are rejected. A missing selected overview is an error.
Missing overview band scale, offset, NoData, unit and description inherit the
parent band's declared metadata; explicitly conflicting numerical metadata is
rejected. The adapter reads parent metadata through the same metered source
registration before opening the selected view. Both metadata opens count in
cold access. This avoids GDAL's overview proxy silently defaulting a parent
scale/offset to1/0; no sample values are changed during raw reading or compilation.

Compiling that source writes its selected grid and exact samples into SKV. The
optional `source_overview` provenance is preserved in raw metadata, protected by
a required format feature flag. The SKV's intrinsic grid is already the chosen
view, so open it without another `overview` selector. Its original TIFF is not
required for serving. An SKV compiled from native resolution cannot substitute
for a pre-existing averaged overview without a separately proved contract.

The adapter uses GDAL's documented existing-view open option; it does not use
RasterIO resizing. See [GDAL dataset open options](https://gdal.org/en/stable/api/gdaldataset_cpp.html).
