# Historical isolated control: source identity failure

The fresh beta2 programme retained 12 numerical failures from the private
historical isolated adapter: both traversal strategies, both 64-zone families,
and all three rounds. All tasks executed and returned the expected shape, but
later logical bands repeated the first band's answers. The 24-date controls
each had 8,188 exact field mismatches; the six-file controls each had 3,812.
Across six tasks of each family this is 72,000 mismatching comparisons.
Neither the historical adapter nor the current product was modified in response.
The original aggregate receipt remains `passed: false`.

Read-only inspection found that the historical adapter's forwarding
`TrackedRaster` instances do not assign distinct upstream source names. The
bounded independent probe in `reference_name_probe.py` reproduces the behavior
using a new minimal forwarding class and two 2x2 arrays. Expected sums are 10
and 100. Without wrapper names, both strategies return 10 and 10; assigning
unique wrapper names restores 10 and 100, and all five expected fields. Array
values, geometry, strategies and operations remain identical. This isolates a
source-identity requirement at the wrapper boundary; it does not blame the
current embedded bridge or establish a new upstream algorithm defect.

The natural reference uses the pinned public source adapters with distinct
names, and the current C++ bridge supplies distinct source identities. Current
native strict and embedded matching-upstream checks passed separately. The
historical timings remain visible as invalid controls, with no speed ratio or
equivalent-work claim. The historical worker has its own process and memory
envelope, which remains different from embedded execution.

Reproduce the small untimed diagnosis from a checkout with the pinned benchmark
dependencies:

```sh
python benchmarks/beta2/reference_name_probe.py --output scratch/reference-names.json
```

The historical adapter is private and is not a product dependency. Its source
is not copied into this report. The minimal public probe above requires only
exactextract 0.3.0 and the existing benchmark dependencies. The report includes
the actual four-case receipt and source hashes. The upstream constructor API is
visible in [pinned raster.py](https://github.com/isciences/exactextract/blob/v0.3.0/python/src/exactextract/raster.py).
