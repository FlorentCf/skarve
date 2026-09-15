# Explicit unscaled Rasterio compatibility

`exactextract_rasterio_v030` is an opt-in numerical policy for the installed
optional exactextract 0.3.0 backend. The default exactextract policy remains
`exactextract_fractional_v030`; native defaults and compiled SKV bytes do not
change. This option requires an enabled build and ordinary `infuse`/`carve` or
`cleave` source access. It does not consume native summary indexes.

```python
with sk.infuse(source_spec) as source:
    result = source.carve(
        zone=polygon, bands=[0], metrics=['sum', 'support', 'mean', 'min', 'max'],
        backend='exactextract', numerical_policy='exactextract_rasterio_v030',
        backend_options={'strategy': 'raster-sequential',
                         'max_cells_in_memory': 262144})
    print(result['provenance'])
```

Node accepts the same `backend`, `numerical_policy` and `backend_options` fields
in `source.carve({...})`; `cleave` jobs carry them at the top level. CLI requests
use `--backend exactextract --numerical-policy exactextract_rasterio_v030`.
The runtime `backends` response advertises both policies in `exactextract_policies`
and retains the original singular legacy `exactextract_policy` field.

The input contract is `rasterio_unscaled_raw_v030`:

- Sources must expose original Float32 or Float64 samples and independent raw
  mask bytes with the all-valid or NoData-derived mask contract (GDAL flags 1
  or 8). Explicit independent masks and alpha masks reject before pixel reads.
  Every exposed band must have scale 1, offset 0 and uniform NoData
  metadata. Source selections with mixed exposed NoData, scaled arrays, integer
  arrays or normalized-only resident arrays fail explicitly. A mapped source is
  an exposed view; comparisons to an original dataset require its same uniform
  NoData contract, band mapping and selected view.
- Nonzero mask bytes mean present. NoData is cast to the source scalar type
  before comparison, as in the pinned upstream Python binding. NaN is invalid.
  NoData infinities are invalid; an otherwise valid infinity rejects
  the complete result because Skarve's result/lease contract requires finite
  valid values. A zero raw mask byte must agree with the declared NoData-derived
  contract; contradictory masks reject the complete result. Negative finite
  values remain valid.
- Finite Float32 values convert exactly to Float64; Float64 values are retained.
  Identity scale/offset is not evaluated as multiplication/addition, preserving
  signed zero. The upstream five common reducers explicitly convert values and
  coverage to double for sums; this policy does not change their accumulation.
- For north-up grids, bounds are evaluated from the original affine transform.
  Coverage resolution is `(right-left)/width`, `(top-bottom)/height`, matching
  `RasterioRasterSource.res()` from exactextract 0.3.0. The native source grid,
  declared CRS, pixel convention, source identity and stored samples remain
  unchanged. Original world-coordinate polygons go to upstream unchanged.
- Multiple inputs still require identical native grids and CRS. There is no
  implicit reprojection, resampling, overview selection or mask replacement.

Results identify the policy, `source_interpretation`, and
`coverage_grid_interpretation: rasterio_bounds_resolution_v030`. Batch
fingerprints include the selected policy and interpretation. New-policy decoded
windows have a separate cache namespace from legacy normalized windows.
A policy conflict or ineligible source produces an error, with no fallback
credit or partial answer.

Raw read scratch plus the Float64/validity output and metadata are admitted
under the existing `window_bytes` budget before each callback fetch. Raw reads
remain cancellable; output is discarded on callback, source-generation or
nonfinite-result failure. This adds no read/window/decoded-byte cap, process
limit or hard cancellation guarantee. See [backends](backends.md) for the
embedded execution limits.

Compatibility qualification must identify the exact upstream/Rasterio/GDAL/GEOS
versions, strategy, maximum cells and source view. The historical local natural
reference used exactextract 0.3.0, Rasterio 1.5.1 and GDAL 3.12.4. The policy name
is a versioned input contract, not a universal bitwise guarantee across other
versions, strategies, chunk sizes or unsupported data. Existing results under
the legacy policy keep their original numerical classification and tolerances.

The explicit-mask exclusion is based on an upstream behavior, not a loss of
native SKV mask support. In pinned exactextract 0.3.0, `RasterStats::process`
creates a `RasterView` when the value and coverage grids differ. Its constructor
copies NoData but not the independent mask, and its accessor calls the underlying
value accessor rather than its mask-aware `get`. A generated nonbinary-grid
multipart fixture demonstrated a masked finite 10000 sample contributing exactly
10000 to sum and one cell to support. Encoding invalid samples as NoData removed
that contribution; unmasked NaN was not the cause. Finite masked values also
changed other shape results. The new policy therefore rejects this source
contract rather than reproducing the incorrect masked answers. Legacy and native
policies retain their existing independent-mask support and semantics. No source
pixels are rewritten to make a query eligible.
