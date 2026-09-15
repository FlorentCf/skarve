#!/usr/bin/env python3
"""One retained source query and one paged batch through the ordinary Skarve API."""
import argparse
import json
from pathlib import Path

from skarve import Skarve

METRICS = ["sum", "support", "mean", "min", "max"]


def run(fixture_path, backend="native", accepted_policies=None):
    fixture_path = Path(fixture_path)
    fixture = json.loads(fixture_path.read_text())
    folder = fixture_path.parent
    selection = {"backend": backend}
    if accepted_policies is not None:
        selection["accepted_policies"] = accepted_policies
    with Skarve() as sk:
        with sk.infuse(folder / fixture["sources"]["original"]["file"]) as source:
            result = source.carve(zone=fixture["geometries"]["small"], bands=[0, 2],
                                  metrics=METRICS, **selection)
        job = {"zones": [{"id": name, "version": "1", "geometry": fixture["geometries"][name]}
                          for name in ("small", "overlap", "thin", "outside")],
               "slices": [{"id": name, "spec": {"location": str(folder / fixture["sources"][name]["file"])},
                           "bands": [0, 2]} for name in ("original", "date-b")],
               "crs": fixture["crs"], "metrics": METRICS, **selection}
        rows, final = 0, None
        for page in sk.cleave(job, max_rows=2):
            # Send these rows to the application's bounded sink before advancing.
            rows += len(page["rows"])
            final = page
        assert rows == 8 and final["complete"]
        return {"single": result, "batch_rows": rows,
                "batch_metrics": final["metrics"],
                "batch_provenance": final.get("provenance")}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--backend", choices=["native", "exactextract", "auto"], default="native")
    parser.add_argument("--accepted-policies", help="Comma-separated explicit accepted policies")
    args = parser.parse_args()
    accepted = args.accepted_policies.split(",") if args.accepted_policies else None
    print(json.dumps(run(args.fixture, args.backend, accepted), allow_nan=False))
