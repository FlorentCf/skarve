"""Independent Fraction oracles through the ordinary resident/file interfaces."""
from fractions import Fraction
from pathlib import Path
import math
import os
import sys
os.environ.setdefault("GDAL_CACHEMAX", "16")
import numpy as np
import pytest
import rasterio
from affine import Affine

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bindings/python"))
from raster_engine_lab import Engine, EngineError


def variance(values, fractions, weights=None):
    pairs = []
    for i, (value, fraction) in enumerate(zip(values, fractions)):
        if value == -9999:
            continue
        weight = Fraction.from_float(fraction)
        if weights is not None:
            if weights[i] < 0:
                continue
            weight *= Fraction.from_float(float(weights[i]))
        pairs.append((Fraction.from_float(float(value)), weight))
    total = sum((w for _, w in pairs), Fraction())
    if not total:
        return None
    mean = sum((x * w for x, w in pairs), Fraction()) / total
    return float(sum((w * (x - mean) ** 2 for x, w in pairs), Fraction()) / total)


@pytest.fixture
def source(tmp_path):
    width = 513  # Crosses three streamed tiles and several persisted leaves.
    col = np.arange(width)
    values = 1e16 + (col % 7) * 2.
    categories = (col % 3).astype(float)
    weights = (col % 11).astype(float)
    values[col % 19 == 0] = -9999
    categories[col % 13 == 0] = -9999
    weights[col % 17 == 0] = -1
    arrays = np.array([values, categories, weights])[:, None, :]
    path = tmp_path / "moments.tif"
    with rasterio.open(path, "w", driver="GTiff", width=width, height=1, count=3,
                       dtype="float64", nodata=-9999, crs="EPSG:3857",
                       transform=Affine(1, 0, 0, 0, -1, 1), tiled=True,
                       blockxsize=32, blockysize=16) as dataset:
        dataset.write(arrays)
    polygon = {"type": "Polygon", "coordinates": [[
        [.25, .25], [width-.25, .25], [width-.25, .75], [.25, .75], [.25, .25]]]}
    fractions = [.375] + [.5] * (width-2) + [.375]
    return path, arrays, polygon, fractions


def test_scalar_streamed_and_persisted_raw_fallback_share_moments(source, tmp_path):
    path, arrays, polygon, fractions = source
    expected = variance(arrays[0, 0], fractions)
    weighted = variance(arrays[0, 0], fractions, arrays[2, 0])
    with Engine() as engine:
        request = dict(bands=[0], statistics=["variance", "stddev", "weighted_variance"], weight_band=2)
        file_answer = engine.measure_file(polygon, "EPSG:3857", path=path, **request)
        raster = dict(grid=dict(width=513, height=1, transform=[0,1,0,1,0,-1], crs="EPSG:3857"),
                      bands=[dict(values=band.ravel().tolist(), valid=(band.ravel()!=-9999).tolist()) for band in arrays])
        engine.open_raster(raster)
        answers = [file_answer, engine.measure(polygon, crs="EPSG:3857", **request)]
        engine.prepare()
        answers.append(engine.measure(polygon, crs="EPSG:3857", **request))
        index = tmp_path / "summary"
        engine.prepare_file(path, index, tile_edge=16, summary_backend="hierarchy", boundary_source="original")
        indexed = engine.measure_file(polygon, "EPSG:3857", path=path, index=index, **request)
        assert indexed["work"]["summary_nodes"] == 0
        answers.append(indexed)
    for answer in answers:
        result = answer["bands"][0]
        assert result["variance"] == pytest.approx(expected, rel=1e-10, abs=0)
        assert result["stddev"] == pytest.approx(math.sqrt(expected), rel=1e-10, abs=0)
        assert result["weighted_variance"] == pytest.approx(weighted, rel=1e-10, abs=0)


def test_streamed_categories_and_exact_ranks_match_fraction_oracle(source):
    path, arrays, polygon, fractions = source
    masses = [sum((Fraction.from_float(f) for value, f in zip(arrays[1,0], fractions) if value == c), Fraction()) for c in range(3)]
    total = sum(masses, Fraction())
    majority = max(range(3), key=lambda c: (masses[c], -c))
    median = next(c for c in range(3) if sum(masses[:c+1], Fraction()) >= total / 2)
    with Engine() as engine:
        answer = engine.measure_file(polygon, "EPSG:3857", path=path, bands=[1],
                                     statistics=["categories", "majority", "variety", "median", "quantiles"],
                                     category_values=[0.,1.,2.], quantiles=[0.,.5,1.])
    result = answer["bands"][0]
    assert result["categories"]["covered_cell_equivalents"] == [float(x) for x in masses]
    assert result["categories"]["fractions"] == [float(x/total) for x in masses]
    assert result["majority"] == majority
    assert result["median"] == median
    assert result["quantiles"]["values"] == [0., median, 2.]


def test_streamed_quantile_budget_is_global_across_tiles(source):
    path, _, polygon, _ = source
    with Engine() as engine:
        with pytest.raises(EngineError, match="sample budget"):
            engine.measure_file(polygon, "EPSG:3857", path=path, bands=[1],
                                statistics=["median"], quantile_max_samples=300)


def test_thin_support_variance_keeps_native_fraction_contract():
    polygon = {"type":"Polygon", "coordinates":[[[.5,-1e-300],[1.5,-1e-300],[1.5,0.],[.5,0.],[.5,-1e-300]]]}
    with Engine() as engine:
        engine.open_raster(dict(grid=dict(width=2,height=1,transform=[0,1,0,0,0,-1],crs="LOCAL"),
                                bands=[dict(values=[-3.,5.])]))
        result = engine.measure(polygon, crs="LOCAL", statistics=["variance","stddev"])["bands"][0]
    # Origin zero preserves both submitted y coordinates through the affine.
    # At origin one, 1 - 1e-300 rounds to 1 and both control/current reject
    # the collapsed ring before reduction; that is not a variance fixture.
    # Each submitted value has the same positive exact-rational tiny support.
    assert result["variance"] == pytest.approx(16., rel=1e-10, abs=0)
    assert result["stddev"] == pytest.approx(4., rel=1e-10, abs=0)


def test_rescaling_cannot_turn_unrepresentable_positive_variance_into_zero():
    polygon = {"type":"Polygon", "coordinates":[[[0.,0.],[3.,0.],[3.,-1.],
               [2.,-1.],[2.,-1e-10],[0.,-1e-10],[0.,0.]]]}
    with Engine() as engine:
        engine.open_raster(dict(grid=dict(width=3,height=1,transform=[0,1,0,0,0,-1],crs="LOCAL"),
                                bands=[dict(values=[0.,1e-160,5e-161])]))
        with pytest.raises(EngineError, match="below binary64 range"):
            engine.measure(polygon, crs="LOCAL", statistics=["variance"])


def test_subnormal_joint_weight_preserves_representable_variance():
    polygon = {"type":"Polygon", "coordinates":[[[0.,0.],[1.3,0.],[1.3,-1.],[0.,-1.],[0.,0.]]]}
    fraction = Fraction.from_float(1.3) - 1
    weight = fraction * Fraction.from_float(1e-320)
    expected = float(Fraction.from_float(1e160)**2 * weight / (1+weight)**2)
    with Engine() as engine:
        engine.open_raster(dict(grid=dict(width=2,height=1,transform=[0,1,0,0,0,-1],crs="LOCAL"),
                                bands=[dict(values=[0.,1e160]),dict(values=[1.,1e-320])]))
        result = engine.measure(polygon, crs="LOCAL", bands=[0], weight_band=1,
                                statistics=["weighted_variance"])["bands"][0]
    assert result["weighted_variance"] == pytest.approx(expected, rel=1e-10, abs=0)
    assert result["moment_diagnostics"]["weighted_rational_fallback"] is True


def test_stddev_uses_independent_decimal_sqrt_before_final_rounding():
    from decimal import Decimal, localcontext
    tiny = float.fromhex("0x0.0000000000001p-1022")
    cases = [[0.,1e-200], [0.,1e-160], [-1e200,1e200],
             [-sys.float_info.max,sys.float_info.max], [-tiny,tiny]]
    polygon = {"type":"Polygon", "coordinates":[[[0.,0.],[2.,0.],[2.,-1.],[0.,-1.],[0.,0.]]]}
    for values in cases:
        # Fraction centers exactly; Decimal handles the final irrational root
        # independently of the engine's binary exponent normalization.
        rational_values = list(map(Fraction.from_float, values))
        mean = sum(rational_values, Fraction()) / 2
        squared = sum(((value-mean)**2 for value in rational_values), Fraction()) / 2
        with localcontext() as ctx:
            ctx.prec = 100
            expected = float((Decimal(squared.numerator)/Decimal(squared.denominator)).sqrt())
        with Engine() as engine:
            engine.open_raster(dict(grid=dict(width=2,height=1,transform=[0,1,0,0,0,-1],crs="LOCAL"),
                                    bands=[dict(values=values),dict(values=[1.,1.])]))
            result = engine.measure(polygon, crs="LOCAL", bands=[0], weight_band=1,
                                    statistics=["stddev","weighted_stddev"])["bands"][0]
        for field in ["stddev","weighted_stddev"]:
            actual = result[field]
            assert actual > 0
            assert abs(actual-expected) <= abs(expected)*1e-10 + 8*math.ulp(expected)
        assert "variance" not in result
