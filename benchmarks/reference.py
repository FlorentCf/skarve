"""Independent numerical oracle and benchmark-only native/exactextract clients.

The oracle intersects whole GEOS polygons with every candidate source cell and
uses math.fsum. It deliberately does not use scanlines, triangulation, ranges,
native debug output, or the implementation's geometry/reduction routines.
"""
from __future__ import annotations

import ctypes
import json
import math
import os
from pathlib import Path

import numpy as np
from shapely.geometry import shape, box


ROOT = Path(__file__).resolve().parents[1]


class Native:
    def __init__(self, library=None):
        default = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target"))) / "release/libraster_engine.so"
        self.lib = ctypes.CDLL(str(library or os.environ.get("RASTER_ENGINE_LIB", default)))
        self.lib.re_new.restype = ctypes.c_void_p
        self.lib.re_call.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        self.lib.re_call.restype = ctypes.c_void_p
        self.lib.re_free_string.argtypes = [ctypes.c_void_p]
        self.lib.re_drop.argtypes = [ctypes.c_void_p]
        self.lib.re_cancel.argtypes = [ctypes.c_void_p]
        self.handle = self.lib.re_new()
        if not self.handle:
            raise RuntimeError("native session allocation failed")

    def raw(self, request):
        if isinstance(request, dict):
            request = json.dumps(request, separators=(",", ":"), allow_nan=False)
        pointer = self.lib.re_call(self.handle, request.encode())
        if not pointer:
            raise RuntimeError("native call returned null")
        try:
            return json.loads(ctypes.string_at(pointer))
        finally:
            self.lib.re_free_string(pointer)

    def call(self, request):
        answer = self.raw(request)
        if not answer["ok"]:
            raise RuntimeError(answer["error"])
        return answer["result"]

    def close(self):
        if self.handle:
            self.lib.re_drop(self.handle)
            self.handle = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def decoded(raster, index):
    band = raster["bands"][index]
    raw = np.array(band["values"], dtype=np.float64)
    valid = np.isfinite(raw)
    if band.get("nodata") is not None:
        valid &= raw != band["nodata"]
    if band.get("valid") is not None:
        valid &= np.array(band["valid"], dtype=bool)
    values = raw * band.get("scale", 1) + band.get("offset", 0)
    valid &= np.isfinite(values)
    return values, valid


def cell_fractions(grid, geometry):
    polygon = shape(geometry)
    assert polygon.is_valid, "oracle accepts valid geometries only"
    if polygon.is_empty:
        return {}, 0.0
    x0, dx, _, y0, _, dy = grid["transform"]
    area = abs(dx * dy)
    result = {}
    xmin, ymin, xmax, ymax = polygon.bounds
    lo_x = max(0, math.floor((xmin - x0) / dx))
    hi_x = min(grid["width"], math.ceil((xmax - x0) / dx))
    lo_y = max(0, math.floor((ymax - y0) / dy))
    hi_y = min(grid["height"], math.ceil((ymin - y0) / dy))
    for row in range(lo_y, hi_y):
        for col in range(lo_x, hi_x):
            cell = box(x0 + col * dx, y0 + (row + 1) * dy, x0 + (col + 1) * dx, y0 + row * dy)
            fraction = polygon.intersection(cell).area / area
            if fraction > 0:
                result[(row, col)] = fraction
    return result, polygon.area / area


def oracle(raster, geometry, bands=None, histogram_edges=None, weight_band=None):
    grid = raster["grid"]
    fractions, polygon_area = cell_fractions(grid, geometry)
    selected = math.fsum(fractions.values())
    outside = max(0.0, polygon_area - selected)
    result = []
    weight_values, weight_valid = decoded(raster, weight_band) if weight_band is not None else (None, None)
    for index in (range(len(raster["bands"])) if bands is None else bands):
        values, valid = decoded(raster, index)
        support = [(row * grid["width"] + col, f) for (row, col), f in fractions.items() if valid[row * grid["width"] + col]]
        covered = math.fsum(f for _, f in support)
        total = math.fsum(float(values[i]) * f for i, f in support)
        sum_abs = math.fsum(abs(float(values[i]) * f) for i, f in support)
        missing = max(0.0, selected - covered)
        status = "empty" if polygon_area == 0 else "outside" if selected == 0 else "no_valid_data" if covered == 0 else "partial" if missing > 1e-10 or outside > 1e-10 else "ok"
        answer = dict(fractional_sum=total, covered_cell_equivalents=covered,
                      selected_cell_equivalents=selected, missing_cell_equivalents=missing,
                      outside_cell_equivalents=outside, intersecting_cell_count=len(fractions),
                      valid_cell_count=len(support), coverage_weighted_mean=total / covered if covered else None,
                      min=min((float(values[i]) for i, _ in support), default=None),
                      max=max((float(values[i]) for i, _ in support), default=None), status=status,
                      _sum_abs=sum_abs)
        if weight_band is not None:
            weighted = [(i, f) for i, f in support if weight_valid[i] and weight_values[i] >= 0]
            denominator = math.fsum(f * float(weight_values[i]) for i, f in weighted)
            numerator = math.fsum(f * float(values[i]) * float(weight_values[i]) for i, f in weighted)
            answer.update(weight_sum=denominator, weighted_sum=numerator,
                          weighted_mean=numerator / denominator if denominator else None)
        if histogram_edges is not None:
            bins = [[] for _ in histogram_edges[:-1]]
            under, over = [], []
            for i, fraction in support:
                value = values[i]
                if value < histogram_edges[0]:
                    under.append(fraction)
                elif value > histogram_edges[-1]:
                    over.append(fraction)
                else:
                    slot = min(int(np.searchsorted(histogram_edges, value, side="right")) - 1, len(bins) - 1)
                    bins[slot].append(fraction)
            answer["histogram"] = dict(edges=histogram_edges, counts=[math.fsum(b) for b in bins], underflow=math.fsum(under), overflow=math.fsum(over))
        result.append(answer)
    return result


def assert_close(actual, expected):
    """SPEC v1 aggregate tolerance. Never relax this from measured results."""
    tolerance = 1e-8 + 1e-10 * expected["_sum_abs"]
    for key, value in expected.items():
        if key.startswith("_"):
            continue
        if key == "histogram":
            observed = actual[key]
            for hkey in ("counts", "underflow", "overflow"):
                assert np.allclose(observed[hkey], value[hkey], atol=1e-8, rtol=1e-10), (hkey, observed, value)
        elif isinstance(value, str) or value is None or key.endswith("_count"):
            assert actual[key] == value, (key, actual[key], value)
        else:
            allowed = tolerance if key in ("fractional_sum", "weighted_sum") else 1e-8 + 1e-10 * abs(value)
            assert abs(actual[key] - value) <= allowed, (key, actual[key], value, allowed)


def exactextract_sources(arrays, transform, nodata=None):
    from exactextract.raster import NumPyRasterSource
    x0, dx, _, y0, _, dy = transform
    h, w = arrays[0].shape
    return [NumPyRasterSource(a, x0, y0 + h * dy, x0 + w * dx, y0,
                             nodata=nodata, name=f"b{index}") for index, a in enumerate(arrays)]


def exactextract_measure(sources, geometry, strategy):
    from exactextract import exact_extract
    features = [{"type": "Feature", "properties": {}, "geometry": geometry}]
    rows = exact_extract(sources, features, ["sum", "count", "mean", "min", "max"],
                         strategy=strategy, max_cells_in_memory=30_000_000)
    properties = rows[0]["properties"]
    result = []
    for index in range(len(sources)):
        prefix = "" if len(sources) == 1 else f"b{index}_"
        def optional_stat(name):
            value = properties[prefix + name]
            return float(value) if value is not None and math.isfinite(value) else None
        result.append({"fractional_sum": properties[prefix + "sum"],
                       "covered_cell_equivalents": properties[prefix + "count"],
                       "coverage_weighted_mean": optional_stat("mean"),
                       "min": optional_stat("min"), "max": optional_stat("max")})
    return result
