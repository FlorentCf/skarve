# Fractional cumulative integration

The optional cumulative representation integrates a piecewise constant raster
field along polygon boundaries. It uses the same native-grid planar coverage
contract as the ordinary engine. Geometry holes and raster boundaries enter
the integration explicitly. This is an implementation note, not a claim of
new mathematical prior art or a public benchmark result.

The implementation in `src/cumulative.rs` bounds arithmetic using outward
rounding of intervals. Queries may split boundary segments at field and row
discontinuities. Ill-conditioned, unrepresentable or over-budget operations
must fail or take an explicitly reported supported fallback; the existence of
a prepared field does not relax the numerical contract. Source identity,
validity masks, preparation cost and retained memory remain part of the query
lifecycle.

Synthetic numerical and guard tests are included in `tests/architecture_cumulative.rs`,
`tests/architecture_cumulative_lane_review.rs` and the source module's unit tests.
The independent oracle tests use supplied values and geometry rather than
native debug output. The private development study is not included in this
release, and its historical timing claims are not presented as reproducible
public evidence.
