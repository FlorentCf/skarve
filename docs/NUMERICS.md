# Numerical contracts

## Native-grid planar fractional coverage

The ordinary polygon API intersects straight polygon edges with native raster
cells in their shared coordinate system. Each cell contributes its covered area
divided by its native cell area. A center test, all-touched selection or an
overview value is a different definition. Holes subtract area and disjoint
MultiPolygon components contribute independently under the validated geometry
contract. The engine does not repair invalid geometry or reproject coordinates.

Raw NoData and masks exclude a value before scale and offset. Supported finite
signed values, including zero, are retained. A band's fractional sum is the sum
of normalized values times positive coverage; support is the sum of valid
coverage; the mean is their ratio. Min/max and valid cell count use positive
coverage, not a minimum coverage threshold. Empty means and extrema are null;
empty sums and valid support are zero. Missing, selected and outside support
remain distinct result fields rather than being silently filled with zero values.

Floating-point arithmetic normally handles coverage. Difficult thin or
ill-conditioned intersections have a bounded exact-rational fallback using the
actual binary64 input coordinates. This preserves representable positive support,
including the protected very thin cases, without claiming exact real arithmetic
for the user's intended decimal coordinates. Additive state preserves
compensation, including through supported summaries and merges. Results are
binary64 and can still exhibit finite-precision rounding. Unrepresentable or
unsafe requested arithmetic fails explicitly; tests do not loosen the result
definition to gain speed.

Extended reducers are explicit: stable population variance and standard
deviation, aligned weights, fixed-bin histograms, exact categorical support and
bounded inverse-CDF quantiles. A quantile uses the first value whose cumulative
effective weight reaches the requested fraction; it is not an interpolated
sample percentile. Domains, alignment and budgets are validated. Prepared
summaries supply only the states they can support; other requested statistics
require original values. No implicit approximation, unbounded spilling or
silently dropped tiny support is introduced.

## Selected-buffer policies

The typed API receives caller-selected values, identities and optional masks;
it does not infer a polygon, source version, overview or spatial model. Selection
is an ordered multiset: repeated indices contribute repeatedly. Windows have
matching ordered band identities and are reduced in the submitted order.

`strict_selected_v1` is the default: signed finite selected values and zero are
valid, sums use compensation, and an otherwise valid nonfinite value is an
error. `hm_demographics_ordered_v1` is an explicit compatibility policy: exclude
masked, raw NoData, nonfinite and negative values in that order, preserve zero,
left-fold IEEE binary64 additions from positive zero within each band's window,
then left-fold the window partial sums in supplied order. That ordering can
differ from a single flat fold and from compensated summation. It is a numerical
specification, not a guarantee that an application's complete demographics
selection, overview, rounding or bucket logic is reproduced.

The typed interface accepts native-endian contiguous float32/float64 values in
the qualified bindings, preserves their dtype through an owned snapshot and
normalizes scalar values when consumed. Its C ABI version and payload budgets
are explicit. See [typed buffers](TYPED_BUFFERS.md) for ownership and resource accounting.

## Spherical policy and comparisons

`hm_straight_lonlat_spherical_v1` is a separate opt-in source-owned policy for a
normalized EPSG:4326 nonnegative population band. Its straight longitude/latitude
boundary integration and footprint fractions differ from planar cell coverage.
It does not choose application datasets, country ordering or demographic bands.
Unsupported spherical inputs fail instead of being routed through planar math.

Comparison engines can differ in coverage precision, summation order, weighted
quantile definitions, masks, scale handling and empty results. The public
benchmark states which fields are comparable, the oracle/tolerance used and
incompatible cases. A faster result with a different definition is not evidence
that the same computation became faster.
