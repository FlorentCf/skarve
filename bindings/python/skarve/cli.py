"""Installed entry point; all CLI computation executes the packaged Rust binary."""
import os
from pathlib import Path
import sys
from raster_engine_lab import EngineError, load_library


def main():
    binary = Path(__file__).resolve().parent / "_native/skarve"
    if not binary.is_file():
        print("Skarve CLI is missing. Reinstall the complete Linux package with native build enabled.", file=sys.stderr)
        raise SystemExit(1)
    try:
        load_library()
    except EngineError as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
    os.execv(str(binary), [str(binary), *sys.argv[1:]])
