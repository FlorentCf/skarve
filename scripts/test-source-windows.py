#!/usr/bin/env python3
"""Run generated source-window, TIFF transport and admission contracts (no timings).

Requires the declared Python test dependencies, Node dependencies and a built
native library. Each invocation creates an isolated temporary fixture directory.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", type=Path, required=True,
                        help="Built native library to test; no implicit artifact selection")
    parser.add_argument("--work", type=Path, required=True,
                        help="Parent for a fresh temporary fixture directory")
    args = parser.parse_args()
    library = args.library.resolve()
    if not library.is_file():
        parser.error(f"native library does not exist: {library}; run scripts/build.py first")
    node = shutil.which("node")
    if not node:
        parser.error("Node is missing; install the documented test prerequisites")
    env = os.environ.copy()
    env["SKARVE_LIBRARY"] = str(library)
    env["PYTHONPATH"] = str(ROOT / "bindings/python") + os.pathsep + env.get("PYTHONPATH", "")
    env["SKARVE_TEST_MODULE"] = (ROOT / "bindings/node/index.mjs").as_uri()
    args.work.mkdir(parents=True, exist_ok=True)

    def run(*command):
        subprocess.run([str(part) for part in command], cwd=ROOT, env=env, check=True)

    def python(test, *arguments):
        run(sys.executable, ROOT / "tests" / test, *arguments)

    with tempfile.TemporaryDirectory(prefix="source-windows-", dir=args.work.resolve()) as directory:
        work = Path(directory)
        fixture = work / "typed-40.tif"
        python("source_buffer.py", "-v")
        python("source_buffer_fixture.py", fixture, "--bands", "40")
        # Exercise the same typed/mask contracts through both public SKV layouts.
        for layout, group in [("band", 1), ("row_group_v1", 40)]:
            run(sys.executable, "-c",
                "from skarve import Skarve; import sys; "
                "e=Skarve(); s=e.infuse(sys.argv[1]); "
                "s.compile(sys.argv[2],chunk_edge=64,band_group=int(sys.argv[4]),payload_layout=sys.argv[3]); "
                "s.close(); e.close()",
                fixture, work / f"{layout}.skv", layout, group)
        for source in [fixture, work / "band.skv", work / "row_group_v1.skv"]:
            run(node, ROOT / "tests/source_buffer_node.mjs", source)
            run(node, ROOT / "tests/source_buffer_node_http.mjs", source)
        python("metadata_prefetch_wide.py", fixture)
        pixel = work / "pixel"
        python("pixel_admission_smoke.py", pixel, "--generate")
        python("pixel_admission_smoke.py", pixel)
        python("pixel_admission_http.py", pixel / "uint32.tif")
        python("metadata_prefetch_fixture.py", pixel)
        python("metadata_prefetch.py", pixel / "rich.cog.tif")
    print("Source-window contracts passed.")


if __name__ == "__main__":
    main()
