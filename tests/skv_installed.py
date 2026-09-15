#!/usr/bin/env python3
"""Generated-only installed SKV compiler/query qualification (no private source).

The containing release runner controls installation and isolation. This script
does not claim an independent host, and never modifies an input outside scratch.
"""
import argparse
import asyncio
import hashlib
import json
import math
from pathlib import Path
import subprocess

import numpy as np
import rasterio
from rasterio.transform import from_origin
from skarve import EngineError, Skarve


METRICS = ["sum", "support", "mean", "min", "max"]


def rectangle(x0, y0, x1, y1):
    return {"type": "Polygon", "coordinates": [[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]]}


def equal(actual, expected):
    assert len(actual) == len(expected)
    for got, want in zip(actual, expected):
        assert set(got) == set(want)
        for key in got:
            if isinstance(got[key], (int, float)) and not isinstance(got[key], bool):
                assert math.isclose(got[key], want[key], rel_tol=2e-13,
                                    abs_tol=0. if 0 < abs(want[key]) < 1e-6 else 1e-10), (key, got[key], want[key])
            else:
                assert got[key] == want[key], key


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=False)
    path = args.scratch / "forty.tif"
    yy, xx = np.indices((65, 67))
    values = np.stack([((17 * xx + 11 * yy + 7 * b) % 251 - 70 + b).astype("float32") for b in range(40)])
    values[:, 3, 4] = -9999
    mask = ((xx + 3 * yy) % 23 != 0).astype("uint8") * 255
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(path, "w", driver="GTiff", width=67, height=65, count=40, dtype="float32",
                           crs="EPSG:3857", transform=from_origin(0, 65, 1, 1), nodata=-9999,
                           tiled=True, blockxsize=16, blockysize=16, compress="deflate", interleave="band") as dataset:
            dataset.write(values)
            dataset.write_mask(mask)
            dataset.scales = tuple(0.5 if b % 2 else 2. for b in range(40))
            dataset.offsets = tuple(b - 3. for b in range(40))
    zones = {"whole": rectangle(0, 0, 67, 65), "edge": rectangle(62.25, -1., 69., 3.75),
             "thin": rectangle(1., 4., 1. + 1e-9, 5.), "outside": rectangle(-4., -4., -1., -1.)}
    original_sha = hashlib.sha256(path.read_bytes()).hexdigest()
    compiled = args.scratch / "forty.skv"
    raw_only = args.scratch / "forty-raw.skv"
    predicted = {codec: args.scratch / ("forty-byte-delta-" + codec + ".skv") for codec in ("none", "deflate")}
    predictor_receipts = {}
    grouped = {codec: args.scratch / ("forty-grouped-" + codec + ".skv") for codec in ("none", "deflate")}
    group_receipts = {}
    ordered_request = {"bands": [39, 0, 17], "polygons": [{"id": "ordered", "windows": [
        {"window": [0, 0, 16, 16], "runs": [[0, 256]]},
        {"window": [1, 1, 16, 16], "runs": [[0, 64], [128, 256]]},
    ]}]}
    with Skarve() as engine:
        backends = engine.call({"op": "backends"})
        references = {}
        exactextract_references = {}
        with engine.infuse(path) as source:
            assert len(source.metadata["metadata"]["bands"]) == 40
            for name, zone in zones.items():
                references[name] = source.carve(zone=zone, metrics=METRICS)["bands"]
                if backends["exactextract"]:
                    exactextract_references[name] = source.carve(zone=zone, metrics=METRICS, backend="exactextract")["bands"]
            built = source.compile(compiled, chunk_edge=64, band_group=4)
            assert built["predictor"] == "none", "the default predictor must remain unchanged"
            assert built["payload_layout"] == "band", "independent-band payloads remain the default"
            ordered_reference = source.sum_selected(ordered_request, numerical_policy="hm_demographics_ordered_v1")["rows"]
            for result, band in zip(ordered_reference[0]["bands"], ordered_request["bands"]):
                expected = 0.
                for window in ordered_request["polygons"][0]["windows"]:
                    x, y, width, _ = window["window"]
                    partial = 0.
                    for start, end in window["runs"]:
                        for index in range(start, end):
                            raw = float(values[band, y + index // width, x + index % width])
                            if math.isfinite(raw) and raw != -9999 and raw >= 0:
                                partial += raw
                    expected += partial
                assert result["sum"] == expected
            assert compiled.is_file()
            try:
                source.compile(compiled)
                raise AssertionError("compile replaced an existing output")
            except EngineError:
                pass
            asyncio.run(source.compile_async(raw_only, chunk_edge=64, band_group=4, summaries=False))
            for codec, output in predicted.items():
                value = source.compile(output, chunk_edge=64, band_group=4, codec=codec, predictor="byte_delta_v1")
                assert value["predictor"] == "byte_delta_v1"
                assert value["logical_digest"] == built["logical_digest"]
                predictor_receipts[codec] = {key: value[key] for key in ("predictor", "byte_length", "logical_digest")}
            for codec, output in grouped.items():
                value = source.compile(output, chunk_edge=64, band_group=40, codec=codec,
                                       predictor="byte_delta_v1", payload_layout="row_group_v1")
                assert value["payload_layout"] == "row_group_v1"
                assert value["logical_digest"] == built["logical_digest"]
                group_receipts[codec] = {key: value[key] for key in ("payload_layout", "byte_length", "logical_digest")}
        assert hashlib.sha256(path.read_bytes()).hexdigest() == original_sha
        # Only this generated fixture is renamed. Serving has no original pathname.
        unavailable = path.with_suffix(".unavailable")
        path.rename(unavailable)
        verified = engine.verify_skv(compiled)
        asyncio.run(engine.verify_skv_async(raw_only))
        for output in (*predicted.values(), *grouped.values()):
            engine.verify_skv(output)
        variants = [(compiled, True), (compiled, False), (raw_only, True)]
        variants.extend((output, use_summaries) for output in predicted.values() for use_summaries in (True, False))
        variants.extend((output, use_summaries) for output in grouped.values() for use_summaries in (True, False))
        for object_path, use_summaries in variants:
            with engine.infuse({"location": str(object_path), "use_summaries": use_summaries}) as source:
                assert source.sum_selected(ordered_request, numerical_policy="hm_demographics_ordered_v1")["rows"] == ordered_reference
                for name, zone in zones.items():
                    equal(source.carve(zone=zone, metrics=METRICS)["bands"], references[name])
                equal(source.carve(zone=zones["whole"], bands=[39], metrics=METRICS)["bands"], [references["whole"][39]])
            for selection in ({}, {"bands": [39]}):
                pages = list(engine.cleave({"zones": [{"id": name, "version": "1", "geometry": zone} for name, zone in zones.items()],
                    "slices": [{"id": "snapshot", "spec": {"location": str(object_path), "use_summaries": use_summaries}, **selection}],
                    "crs": "EPSG:3857", "metrics": METRICS}, max_rows=2))
                assert pages[-1]["complete"] and sum(len(p["rows"]) for p in pages) == len(zones)
                for page in pages:
                    for row in page["rows"]:
                        expected = references[row["zone_id"]]
                        equal(row["bands"], [expected[39]] if selection else expected)
        if backends["exactextract"]:
            for object_path in (compiled, *predicted.values(), *grouped.values()):
                with engine.infuse(object_path) as source:
                    result = source.carve(zone=zones["whole"], metrics=METRICS, backend="exactextract")
                    assert result["provenance"]["selected_backend"] == "exactextract"
                    # Integer-aligned coverage makes this particular oracle compatible.
                    fields = ["band", "fractional_sum", "covered_cell_equivalents", "coverage_weighted_mean", "min", "max"]
                    equal([{key: band[key] for key in fields} for band in result["bands"]],
                          [{key: band[key] for key in fields} for band in references["whole"]])
                    for selection in ({}, {"bands": [39]}):
                        for name, zone in zones.items():
                            expected = exactextract_references[name]
                            equal(source.carve(zone=zone, metrics=METRICS, backend="exactextract", **selection)["bands"],
                                  [expected[39]] if selection else expected)
                for strategy in ("feature-sequential", "raster-sequential"):
                    for selection in ({}, {"bands": [39]}):
                        pages = list(engine.cleave({
                            "zones": [{"id": name, "version": "1", "geometry": zone} for name, zone in zones.items()],
                            "slices": [{"id": "snapshot", "spec": {"location": str(object_path)}, **selection}],
                            "crs": "EPSG:3857", "metrics": METRICS, "backend": "exactextract",
                            "backend_options": {"strategy": strategy},
                        }, max_rows=2))
                        assert pages[-1]["complete"] and sum(len(p["rows"]) for p in pages) == len(zones)
                        for page in pages:
                            assert page["provenance"]["selected_backend"] == "exactextract"
                            for row in page["rows"]:
                                expected = exactextract_references[row["zone_id"]]
                                equal(row["bands"], [expected[39]] if selection else expected)
    fixture = args.scratch / "fixture.json"
    fixture.write_text(json.dumps({"source": str(compiled), "original_unavailable": str(path),
                                   "original_for_compile": str(unavailable), "zones": zones,
                                   "reference": references, "exactextract": backends["exactextract"],
                                   "ordered_request": ordered_request, "ordered_reference": ordered_reference,
                                   "exactextract_reference": exactextract_references}, allow_nan=False))
    zone_path = args.scratch / "zone.json"
    zone_path.write_text(json.dumps(zones["whole"]))
    def cli(*arguments):
        result = subprocess.run([str(args.binary), *map(str, arguments)], text=True, capture_output=True, check=True, timeout=120)
        value = json.loads(result.stdout)
        assert value["ok"]
        return value["result"]
    cli("verify-skv", compiled)
    equal(cli("carve", compiled, zone_path, "--crs", "EPSG:3857", "--metrics", ",".join(METRICS))["bands"], references["whole"])
    cli_compiled = args.scratch / "cli.skv"
    cli("compile", unavailable, "--output", cli_compiled, "--chunk-edge", "64", "--band-group", "4", "--no-summaries")
    cli("verify-skv", cli_compiled)
    for codec in ("none", "deflate"):
        output = args.scratch / ("cli-byte-delta-" + codec + ".skv")
        value = cli("compile", unavailable, "--output", output, "--chunk-edge", "64",
                    "--predictor", "byte_delta_v1", "--codec", codec)
        assert value["predictor"] == "byte_delta_v1"
        cli("verify-skv", output)
        equal(cli("carve", output, zone_path, "--crs", "EPSG:3857", "--metrics", ",".join(METRICS))["bands"], references["whole"])
    for codec in ("none", "deflate"):
        output = args.scratch / ("cli-grouped-" + codec + ".skv")
        value = cli("compile", unavailable, "--output", output, "--chunk-edge", "64", "--band-group", "40",
                    "--predictor", "byte_delta_v1", "--codec", codec, "--payload-layout", "row_group_v1")
        assert value["payload_layout"] == "row_group_v1"
        cli("verify-skv", output)
        equal(cli("carve", output, zone_path, "--crs", "EPSG:3857", "--metrics", ",".join(METRICS))["bands"], references["whole"])
    print(json.dumps({"passed": True, "bands": 40, "geometries": len(zones), "quad_cases": ["1x1", "Nx1", "1xM", "NxM"],
                      "original_path_unavailable": not path.exists(), "compiled_bytes": compiled.stat().st_size,
                      "raw_only_bytes": raw_only.stat().st_size, "compilation": built, "verification": verified,
                      "predictor_receipts": predictor_receipts,
                      "group_receipts": group_receipts,
                      "exactextract": backends["exactextract"], "fixture": str(fixture)}, allow_nan=False))


if __name__ == "__main__":
    main()
