#!/usr/bin/env python3
"""Compile a generated TIFF and consume a complete self-contained SKV result.

Run generate_fixtures.py first. Uses the installed Skarve package; no private data.
The output must not exist. SKV v0 is experimental and is not a permanent format.
"""
import argparse
import json
from pathlib import Path

from skarve import Skarve


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--predictor", choices=["none", "byte_delta_v1"], default="none")
    parser.add_argument("--payload-layout", choices=["band", "row_group_v1"], default="band")
    args = parser.parse_args()
    fixture = json.loads(args.fixture.read_text())
    original = args.fixture.parent / fixture["sources"]["original"]["file"]
    with Skarve() as engine:
        with engine.infuse(original) as source:
            compilation = source.compile(args.output, chunk_edge=64, band_group=4,
                                         predictor=args.predictor, payload_layout=args.payload_layout)
            assert compilation["predictor"] == args.predictor
            assert compilation["payload_layout"] == args.payload_layout
        verified = engine.verify_skv(args.output)
        with engine.infuse(args.output) as source:
            result = source.carve(zone=fixture["geometries"]["small"], metrics=["sum", "support", "mean", "min", "max"])
        pages = list(engine.cleave({
            "zones": [{"id": name, "version": "1", "geometry": fixture["geometries"][name]}
                      for name in ("small", "overlap")],
            "slices": [{"id": "snapshot", "spec": {"location": str(args.output)}}],
            "crs": fixture["crs"], "metrics": ["sum", "support", "mean", "min", "max"],
        }, max_rows=1))
        assert pages[-1]["complete"] and sum(len(page["rows"]) for page in pages) == 2
        print(json.dumps({"compilation": compilation, "verification": verified,
                          "single": result, "batch_rows": 2, "batch_metrics": pages[-1]["metrics"]}, allow_nan=False))


if __name__ == "__main__":
    main()
