#!/usr/bin/env python3
"""End-to-end Rust/GDAL remote COG test with controlled HTTP faults, entirely local."""
from __future__ import annotations
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from range_server import RangeServer


def main():
    import numpy as np
    import rasterio
    from rasterio.shutil import copy as raster_copy
    from rasterio.transform import from_origin

    cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo")
    env = os.environ.copy()
    # Normal libgdal-dev installations discover their own ABI via gdal-config.
    # The original Ubuntu runtime-only host needs the project-local link made by
    # scripts/build.sh; do not pin a different clean Linux environment to 3.8.4.
    runtime = Path("/usr/lib/x86_64-linux-gnu/libgdal.so.34")
    fallback = Path.home() / ".local/lib/raster-engine-lab"
    if not shutil.which("gdal-config") and runtime.is_file() and (fallback / "libgdal.so").is_file():
        env.setdefault("GDAL_LIB_DIR", str(fallback))
        env.setdefault("GDAL_VERSION", "3.8.4")
    results = []
    with tempfile.TemporaryDirectory(prefix="raster-engine-remote-") as folder:
        source, cog = Path(folder) / "source.tif", Path(folder) / "source.cog.tif"
        values = np.arange(1024 * 1024, dtype="float64").reshape(1024, 1024) / 16. - 1000.
        with rasterio.open(source, "w", driver="GTiff", width=1024, height=1024, count=1,
                           dtype="float64", crs="EPSG:3857", transform=from_origin(0,1024,1,1),
                           tiled=True, blockxsize=128, blockysize=128) as dataset:
            dataset.write(values, 1)
        raster_copy(source, cog, driver="COG", BLOCKSIZE=128, COMPRESS="NONE", OVERVIEWS="NONE")
        for mode in ["ok", "ignored", "changed", "missing", "wrong_range", "truncated"]:
            server = RangeServer(cog, mode)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            run_env = env | {"RASTER_TEST_REMOTE_URL": server.url}
            if mode != "ok":
                run_env["RASTER_TEST_REMOTE_FAILURE"] = "1"
            else:
                run_env.pop("RASTER_TEST_REMOTE_FAILURE", None)
            try:
                run = subprocess.run([cargo,"test","--test","io_tests",
                                      "remote_cog_window_from_controlled_server","--","--exact","--ignored","--nocapture"],
                                     cwd=ROOT, env=run_env, text=True, capture_output=True, timeout=90)
                if run.returncode:
                    raise AssertionError(f"remote mode {mode} failed:\n{run.stdout}\n{run.stderr}")
                metrics = None
                for line in run.stdout.splitlines():
                    if "REMOTE_METRICS " in line:
                        metrics = json.loads(line.split("REMOTE_METRICS ", 1)[1])
                if mode == "ok":
                    assert metrics and metrics["accepted_bytes"] < cog.stat().st_size / 4
                    assert metrics["requests"] == len(server.records)
                assert all(record.get("error") != "non-range GET" for record in server.records)
                results.append({"mode":mode, "passed":True, "source_bytes":cog.stat().st_size,
                                "metrics":metrics,"server_requests":server.records})
            finally:
                server.shutdown()
                thread.join(timeout=3)
                server.server_close()
    print(json.dumps({"test":"controlled_remote_cog_window", "results":results}, indent=2))


if __name__ == "__main__":
    main()
