"""Contract-derived GEOS oracle, analytical fixtures and bounded seeded fuzz."""
import copy
import json
import math
import sys
from pathlib import Path

import numpy as np
import pytest
from hypothesis import given, settings, strategies as st
from shapely.geometry import Polygon, MultiPolygon, box, mapping
from shapely.affinity import affine_transform

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from benchmarks.reference import Native, oracle, cell_fractions, assert_close


def raster(size=8, count=3):
    rng = np.random.default_rng(42)
    bands = []
    for index in range(count):
        values = rng.integers(-10, 30, size=size * size).astype(float)
        valid = rng.uniform(size=size * size) > index * .1
        bands.append({"values": values.tolist(), "valid": valid.tolist()})
    return {"grid": {"width": size, "height": size, "transform": [0, 1, 0, size, 0, -1], "crs": "LOCAL"}, "bands": bands}


@pytest.fixture
def native():
    with Native() as client:
        yield client


def run_case(native, data, geometry, *, prepare=False, debug=False, **options):
    native.raw({"op": "close", "source": "r"})
    native.call({"op": "open", "id": "r", "raster": data})
    if prepare:
        native.call({"op": "prepare", "source": "r"})
    expected = oracle(data, geometry, **options)
    answers = []
    for strategy in ("direct", "scanline"):
        answer = native.call({"op": "measure", "source": "r", "geometry": geometry, "crs": data["grid"]["crs"], "strategy": strategy, **options})
        assert len(answer["bands"]) == len(expected)
        for got, want in zip(answer["bands"], expected):
            assert_close(got, want)
        answers.append(answer)
    if debug:
        native._debug_sequence = getattr(native, "_debug_sequence", 0) + 1
        plan = native.call({"op": "compile", "source": "r", "id": f"debug{native._debug_sequence}", "geometry": geometry, "crs": data["grid"]["crs"], "debug_cells": True})
        expected_cells, _ = cell_fractions(data["grid"], geometry)
        observed_cells = {(cell["row"], cell["col"]): cell["fraction"] for cell in plan["cells"]}
        for cell in observed_cells.keys() | expected_cells.keys():
            assert abs(observed_cells.get(cell, 0) - expected_cells.get(cell, 0)) <= 1e-9, cell
    return answers


@pytest.mark.parametrize("geometry", [
    box(1, 1, 7, 6), box(.15, .24, .8, .91), box(-2, 3, 3, 11), box(8, 0, 9, 1),
    Polygon([(0, 0), (8, 0), (0, 8), (0, 0)]),
    Polygon([(0.1, 0.1), (7.9, 0.1), (7.9, 7.9), (.1, 7.9)], [[(2, 2), (2, 6), (6, 6), (6, 2)]]),
    MultiPolygon([box(.1, .2, 2.5, 3.3), box(5, 5, 7.2, 7.3)]),
    Polygon([(0, 0), (7, 7), (7, 7.000001), (0, .000001), (0, 0)]),
    Polygon(),
])
@pytest.mark.parametrize("prepare", [False, True])
def test_shapes_masks_statistics(native, geometry, prepare):
    data = raster()
    data["bands"][0]["values"][0] = None
    data["bands"][0]["nodata"] = -10
    data["bands"][0].update(scale=2.5, offset=-3.0)
    run_case(native, data, mapping(geometry), prepare=prepare,
             histogram_edges=[-20, 0, 10, 20, 60], weight_band=2, debug=True)


def test_analytical_unit_grid(native):
    fixture = json.loads((Path(__file__).parents[2] / "fixtures/analytical.json").read_text())
    for case in fixture["cases"]:
        answers = run_case(native, fixture["raster"], case["geometry"], debug=True)
        for answer in answers:
            result = answer["bands"][0]
            for key, value in case["expected"].items():
                assert result[key] == pytest.approx(value, abs=1e-10), case["name"]


@given(st.integers(0, 2**32 - 1))
@settings(max_examples=70, deadline=None, derandomize=True)
def test_seeded_radial_polygon_fuzz(seed):
    rng = np.random.default_rng(seed)
    n = int(rng.integers(3, 35))
    angles = np.arange(n) * (2 * math.pi / n)
    radii = rng.uniform(.1, 5.5, n)
    coordinates = np.column_stack((4 + np.cos(angles) * radii, 4 + np.sin(angles) * radii))
    polygon = Polygon(coordinates)
    assert polygon.is_valid
    with Native() as native:
        run_case(native, raster(), mapping(polygon), prepare=bool(seed % 2), debug=True)


def test_partition_ring_and_multiband_properties(native):
    data = raster(size=16)
    geometry = Polygon([(1.1, 1.2), (14.5, 2.7), (11, 13.3), (3, 15.1)])
    answers = run_case(native, data, mapping(geometry), prepare=True)[1]
    rotated = list(geometry.exterior.coords)[:-1]
    rotated = rotated[2:] + rotated[:2]
    reversed_geometry = mapping(Polygon(list(reversed(rotated))))
    changed = native.call({"op": "measure", "source": "r", "geometry": reversed_geometry, "crs": "LOCAL"})
    for a, b in zip(answers["bands"], changed["bands"]):
        assert a["fractional_sum"] == pytest.approx(b["fractional_sum"], abs=1e-8)
    sums = [0.0] * 3
    coverages = [0.0] * 3
    for tile in [box(0, 0, 8, 8), box(8, 0, 16, 8), box(0, 8, 8, 16), box(8, 8, 16, 16)]:
        part = geometry.intersection(tile)
        answer = native.call({"op": "measure", "source": "r", "geometry": mapping(part), "crs": "LOCAL"})
        for i, band in enumerate(answer["bands"]):
            sums[i] += band["fractional_sum"]
            coverages[i] += band["covered_cell_equivalents"]
    native.call({"op": "compile", "source": "r", "id": "p", "geometry": mapping(geometry), "crs": "LOCAL"})
    reused = native.call({"op": "measure", "source": "r", "plan": "p"})
    for i, band in enumerate(answers["bands"]):
        assert sums[i] == pytest.approx(band["fractional_sum"], abs=1e-8)
        assert coverages[i] == pytest.approx(band["covered_cell_equivalents"], abs=1e-8)
        separate = native.call({"op": "measure", "source": "r", "geometry": mapping(geometry), "crs": "LOCAL", "bands": [i]})
        assert separate["bands"][0] == reused["bands"][i]


@pytest.mark.parametrize("magnitude", [1e8, 1e14, 1e16])
def test_cancellation_numerical_conditioning(native, magnitude):
    data = raster(size=64, count=1)
    data["bands"][0]["values"] = ([magnitude, 1, -magnitude, -2] * 1024)
    for geometry in (box(0, 0, 64, 64), box(.25, 2.2, 63.1, 63.8)):
        run_case(native, data, mapping(geometry), prepare=True)


@pytest.mark.parametrize("transform", [[500000, .25, 0, 6000000, 0, -.5], [0.2, 25, 0, 500, 0, -40]])
def test_native_resolution_transform(native, transform):
    data = raster()
    data["grid"]["transform"] = transform
    x0, dx, _, y0, _, dy = transform
    shape_in_pixels = Polygon([(.25, .2), (7.8, 2.5), (2.5, 7.2)])
    geometry = affine_transform(shape_in_pixels, [dx, 0, 0, dy, x0, y0])
    run_case(native, data, mapping(geometry), prepare=True, debug=True)


@pytest.mark.parametrize("payload", [
    "garbage", "{", "[]", "null", '{"op":"bogus"}', '{"op":"stats","x":NaN}',
    {"op": "measure", "source": "missing", "geometry": {"type": "Point", "coordinates": [0, 0]}, "crs": "LOCAL"},
])
def test_malformed_requests_recover(native, payload):
    assert native.raw(payload)["ok"] is False
    assert native.raw({"op": "stats"})["ok"] is True


@pytest.mark.parametrize("geometry", [
    {"type": "Polygon", "coordinates": [[[0, 0], [3, 3], [3, 0], [0, 3], [0, 0]]]},
    mapping(MultiPolygon([box(1, 1, 4, 4), box(2, 2, 5, 5)])),
    {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [0, 1]]]},
    {"type": "Polygon", "coordinates": [[[0, 0], [1e30, 0], [0, 1], [0, 0]]]},
])
def test_invalid_geometries(native, geometry):
    native.call({"op": "open", "id": "r", "raster": raster()})
    assert not native.raw({"op": "measure", "source": "r", "geometry": geometry, "crs": "LOCAL"})["ok"]


def test_grid_identity_and_mask_independence(native):
    data = raster(count=2)
    native.call({"op": "open", "id": "r", "raster": data})
    native.call({"op": "compile", "source": "r", "id": "p", "geometry": mapping(box(.1, .2, 7, 7)), "crs": "LOCAL"})
    shifted = copy.deepcopy(data)
    shifted["grid"]["transform"][0] += .5
    native.call({"op": "open", "id": "s", "raster": shifted})
    assert not native.raw({"op": "measure", "source": "s", "plan": "p"})["ok"]
    assert not native.raw({"op": "measure", "source": "r", "geometry": mapping(box(0, 0, 1, 1)), "crs": "EPSG:3857"})["ok"]


def test_same_grid_changed_values_reuses_only_geometry(native):
    data = raster(count=1)
    geometry = mapping(box(.12, .25, 7.51, 7.25))
    native.call({"op": "open", "id": "original", "raster": data})
    native.call({"op": "compile", "source": "original", "id": "shared", "geometry": geometry, "crs": "LOCAL"})
    changed = copy.deepcopy(data)
    changed["bands"][0]["values"] = [value * -2 + 3 for value in data["bands"][0]["values"]]
    changed["bands"][0]["valid"][5] = False
    native.call({"op": "open", "id": "changed", "raster": changed})
    native.call({"op": "prepare", "source": "changed"})
    answer = native.call({"op": "measure", "source": "changed", "plan": "shared"})
    assert_close(answer["bands"][0], oracle(changed, geometry)[0])


def test_no_valid_and_zero_weight_support(native):
    data = raster(count=2)
    data["bands"][0]["valid"] = [False] * 64
    data["bands"][1]["values"] = [0] * 64
    run_case(native, data, mapping(box(0, 0, 8, 8)), weight_band=1, histogram_edges=[0, 1])
    data["bands"][0]["valid"] = [True] * 64
    run_case(native, data, mapping(box(0, 0, 8, 8)), weight_band=1)


@pytest.mark.parametrize("edges", [[0, 0, 1], [1, 0], [0], list(range(258))])
def test_invalid_histogram_edges(native, edges):
    native.call({"op": "open", "id": "r", "raster": raster()})
    assert not native.raw({"op": "measure", "source": "r", "geometry": mapping(box(0, 0, 8, 8)), "crs": "LOCAL", "histogram_edges": edges})["ok"]


def test_nonfinite_arithmetic_fails(native):
    data = raster(count=1)
    data["bands"][0]["values"] = [1e308] * 64
    native.call({"op": "open", "id": "r", "raster": data})
    for strategy in ("direct", "scanline"):
        assert not native.raw({"op": "measure", "source": "r", "geometry": mapping(box(0, 0, 8, 8)), "crs": "LOCAL", "strategy": strategy})["ok"]


@pytest.mark.parametrize("geometry", [box(179, 0, 181, 1), box(0, 84, 1, 86), Polygon([(179, 0), (-179, 0), (-179, 1), (179, 1)])])
def test_unsupported_geographic_queries_fail(native, geometry):
    data = raster(count=1)
    data["grid"].update(transform=[0, 1, 0, 8, 0, -1], crs="EPSG:4326")
    native.call({"op": "open", "id": "r", "raster": data})
    assert not native.raw({"op": "measure", "source": "r", "geometry": mapping(geometry), "crs": "EPSG:4326"})["ok"]


@pytest.mark.parametrize("width", [1e-8, 1e-10, 1e-12])
def test_positive_coverage_is_not_zero_by_threshold(native, width):
    data = raster(count=1)
    data["bands"][0]["values"] = [1e6] * 64
    run_case(native, data, mapping(box(.1, .5, 7.9, .5 + width)), prepare=True, debug=True)


def test_touching_component_corners_and_hole_vertex(native):
    data = raster()
    for geometry in (
        MultiPolygon([box(.1, .1, 3, 3), box(3, 3, 7.1, 7.1)]),
        Polygon([(0, 0), (8, 0), (8, 8), (0, 8)], [[(0, 4), (2, 3), (2, 5), (0, 4)]])
    ):
        assert geometry.is_valid
        run_case(native, data, mapping(geometry), debug=True)


def test_twenty_independently_masked_aligned_bands(native):
    data = raster(size=8, count=20)
    rng = np.random.default_rng(71717)
    for band in data["bands"]:
        band["valid"] = (rng.random(64) > .15).tolist()
    run_case(native, data, mapping(Polygon([(.3, .1), (7.8, 2.4), (5.6, 7.7), (1.2, 5.1)])), prepare=True, weight_band=19)


@pytest.mark.parametrize("transform", [[0, -1, 0, 8, 0, -1], [0, 1, .1, 8, 0, -1], [0, 1, 0, 8, .2, -1], [0, 1, 0, 8, 0, 1], [0, 0, 0, 8, 0, -1]])
def test_unsupported_grids_rejected(native, transform):
    data = raster()
    data["grid"]["transform"] = transform
    assert not native.raw({"op": "open", "id": "r", "raster": data})["ok"]
