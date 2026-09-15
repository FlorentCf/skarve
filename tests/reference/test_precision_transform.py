"""Conservative exact-fallback bounds after binary64 affine normalization."""
from fractions import Fraction
import math
from benchmarks.reference import Native


def test_normalization_rounding_does_not_omit_positive_boundary_cell():
    # The rounded quotient is2.0, but the exact ratio of these submitted binary64
    # values is slightly below2. Tiny positive support in col1 affects min/mean.
    f = Fraction.from_float
    assert (0.5 - 0.1) / 0.2 == 2.0
    assert (f(0.5) - f(0.1)) / f(0.2) < 2
    grid = dict(width=4, height=4, transform=[0.1,0.2,0,0.9,0,-0.2], crs="LOCAL")
    x0, x1, y0, y1 = 0.5, 0.500000000001, 0.31, 0.69
    ring = [[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]
    values = [7.0 if col == 1 else 100.0 for row in range(4) for col in range(4)]
    fractions = {}
    left = (f(x0)-f(0.1))/f(0.2)
    right = (f(x1)-f(0.1))/f(0.2)
    top = (f(y1)-f(0.9))/f(-0.2)
    bottom = (f(y0)-f(0.9))/f(-0.2)
    for row in range(4):
        for col in range(4):
            dx = max(Fraction(0), min(right,col+1)-max(left,col))
            dy = max(Fraction(0), min(bottom,row+1)-max(top,row))
            if dx*dy > 0:
                fractions[row,col] = dx*dy
    assert len(fractions) == 4 and (1,1) in fractions
    exact_sum = sum(f(values[row*4+col])*fraction for (row,col),fraction in fractions.items())
    exact_support = sum(fractions.values())
    exact_mean = float(exact_sum/exact_support)
    with Native() as native:
        native.call(dict(op="open",id="r",raster=dict(grid=grid,bands=[dict(values=values)])))
        for strategy in ["direct","scanline"]:
            result = native.call(dict(op="measure",source="r",geometry=dict(type="Polygon",coordinates=[ring]),crs="LOCAL",strategy=strategy))
            band = result["bands"][0]
            assert band["intersecting_cell_count"] == 4
            assert band["min"] == 7.0
            assert band["max"] == 100.0
            assert abs(band["fractional_sum"]-float(exact_sum)) <= 1e-8+1e-10*abs(float(exact_sum))
            assert abs(band["coverage_weighted_mean"]-exact_mean) <= 1e-8+1e-10*abs(exact_mean)
            assert math.isclose(band["covered_cell_equivalents"],float(exact_support),rel_tol=1e-12,abs_tol=1e-25)
