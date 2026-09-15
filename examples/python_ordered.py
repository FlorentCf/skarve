#!/usr/bin/env python3
"""Generated-only ordered raw selections and an independent arithmetic oracle.

This expert selection API does not create polygons or application buckets.
The fractional comparison uses the same rectangle with its separate default
mask/scale/offset policy; its answer is intentionally different.
"""
import argparse
import asyncio
import hashlib
import json
import math
from pathlib import Path
import shutil
import struct

from skarve import Skarve

POLICY = "hm_demographics_ordered_v1"
MAPPING = [2, 0, 1]
REQUEST = {
    "bands": [1, 0],
    "polygons": [
        {"id": "rectangle", "windows": [{"window": [2, 3, 9, 5], "runs": [[0, 45]]}]},
        {"id": "shifted-overlap", "windows": [
            {"window": [0, 0, 8, 5], "runs": [[0, 8], [10, 17], [24, 33]]},
            {"window": [3, 2, 8, 5], "indexes": [0, 2, 3, 8, 9, 15, 18, 24, 25, 31, 39]},
            {"window": [1, 1, 6, 4], "runs": [[2, 9], [12, 17], [20, 24]]},
        ]},
        {"id": "empty", "windows": [{"window": [0, 0, 4, 4], "runs": []}]},
    ],
}
RECTANGLE = {"type": "Polygon", "coordinates": [[[2, 56], [11, 56], [11, 61], [2, 61], [2, 56]]]}


def sample(band, x, y):
    """Reconstruct generate_fixtures.py algebra, without reading raster values."""
    value = -9999 if (x + 2 * y + band) % 31 == 0 else (x + 3 * y + 11 * band) % 61 - 20 + 2 * band
    return struct.unpack("<f", struct.pack("<f", value))[0]


def oracle():
    rows = []
    for polygon in REQUEST["polygons"]:
        bands = []
        for selected in REQUEST["bands"]:
            original = MAPPING[selected]
            result = dict(id=selected, sum=0.0, has_values=False, valid_count=0,
                          excluded_mask=0, excluded_nodata=0, excluded_nonfinite=0, excluded_negative=0)
            for window in polygon["windows"]:
                x, y, width, _ = window["window"]
                indexes = window.get("indexes")
                if indexes is None:
                    indexes = (i for start, end in window["runs"] for i in range(start, end))
                partial = 0.0
                for index in indexes:
                    value = sample(original, x + index % width, y + index // width)
                    if not math.isfinite(value):
                        result["excluded_nonfinite"] += 1
                    elif value == -9999:
                        result["excluded_nodata"] += 1
                    elif value < 0:
                        result["excluded_negative"] += 1
                    else:
                        result["valid_count"] += 1
                        partial += value
                # Keep physical fetches independent from this logical-window fold.
                result["sum"] += partial
            result["has_values"] = result["valid_count"] > 0
            bands.append(result)
        rows.append({"id": polygon["id"], "bands": bands})
    return rows


def normalized_rectangle_oracle():
    expected = []
    scales, offsets = [.5, 2., -1.], [10., -4., 3.]
    for selected in REQUEST["bands"]:
        original = MAPPING[selected]
        values = [sample(original, x, y) * scales[original] + offsets[original]
                  for y in range(3, 8) for x in range(2, 11)
                  if (x + y) % 17 != 0 and sample(original, x, y) != -9999]
        expected.append({"band": selected, "fractional_sum": math.fsum(values),
                         "covered_cell_equivalents": len(values)})
    return expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    args = parser.parse_args()
    fixture = json.loads(args.fixture.read_text())
    assert fixture["origin"] == "Deterministically generated mathematical test data; no external dataset."
    original = args.fixture.parent / fixture["sources"]["original"]["file"]
    original_sha = hashlib.sha256(original.read_bytes()).hexdigest()
    assert original_sha == fixture["sources"]["original"]["sha256"]
    args.scratch.mkdir(parents=True, exist_ok=False)
    owned = args.scratch / "generated-original.tif"
    shutil.copy2(original, owned)
    compiled = args.scratch / "ordered.skv"
    expected = oracle()
    with Skarve() as engine:
        with engine.infuse({"location": str(owned), "bands": MAPPING}) as source:
            direct = source.sum_selected(REQUEST, numerical_policy=POLICY)
            assert direct["complete"] and direct["numerical_policy"] == POLICY
            assert direct["rows"] == expected
            fractional = source.carve(zone=RECTANGLE, bands=REQUEST["bands"], metrics=["sum", "support"])
            for got, want in zip(fractional["bands"], normalized_rectangle_oracle()):
                assert all(got[key] == value for key, value in want.items())
            assert [b["fractional_sum"] for b in fractional["bands"]] != [b["sum"] for b in expected[0]["bands"]]
            source.compile(compiled, chunk_edge=64, predictor="byte_delta_v1")
        # Only the example-owned generated copy is renamed; the input fixture stays intact.
        owned.rename(owned.with_suffix(".unavailable"))
        assert not owned.exists()
        with engine.infuse(compiled) as source:
            serving = source.sum_selected(REQUEST, numerical_policy=POLICY)
            asynchronous = asyncio.run(source.sum_selected_async(REQUEST, numerical_policy=POLICY))
            assert serving["rows"] == asynchronous["rows"] == expected
            assert serving["provenance"]["summaries_used"] is False
            assert serving["provenance"]["source_layout"]["self_contained"] is True
    assert hashlib.sha256(original.read_bytes()).hexdigest() == original_sha
    request_path = args.scratch / "selections.json"
    request_path.write_text(json.dumps(REQUEST, indent=2) + "\n")
    print(json.dumps({"passed": True, "policy": POLICY, "source_mapping": MAPPING,
                      "rows": expected, "source_original_unavailable": not owned.exists(),
                      "source": str(compiled.resolve()), "selection": str(request_path.resolve()),
                      "fractional_normalized_rectangle": normalized_rectangle_oracle(),
                      "comparison_contract": "Different raw ordered and fractional normalized policies; not a compatibility or speed claim.",
                      "fixture_rounding_scope": "These small Float32 integers sum exactly in binary64; rounding-sensitive window-order tests are separate.",
                      "direct_metrics": direct["metrics"], "skv_metrics": serving["metrics"]}, allow_nan=False))


if __name__ == "__main__":
    main()
