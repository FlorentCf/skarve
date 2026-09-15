#!/usr/bin/env python3
"""Build unmodified optional upstream core and Skarve's C ABI bridge on Linux."""
import argparse
import ctypes
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys

from fetch import PIN, ROOT, verified_source


def run(command):
    subprocess.run([str(x) for x in command], check=True)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--profile", choices=["debug", "release"], default="release")
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--shared-control", action="store_true")
    args = parser.parse_args()
    if sys.platform != "linux":
        raise RuntimeError("optional exactextract is currently qualified only on Linux")
    if not 1 <= args.jobs <= 2:
        raise RuntimeError("optional exactextract build permits one or two jobs")
    source = verified_source()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    cmake = os.environ.get("SKARVE_CMAKE") or shutil.which("cmake")
    cxx = os.environ.get("CXX") or shutil.which("c++")
    if not cmake or not cxx:
        raise RuntimeError("optional exactextract build requires CMake >=3.15 and a C++17 compiler")
    include = Path(os.environ.get("SKARVE_GEOS_INCLUDE_DIR", "/usr/include")).resolve()
    header = include / "geos_c.h"
    if not header.is_file():
        raise RuntimeError("GEOS development header missing; install matching libgeos-dev or set SKARVE_GEOS_INCLUDE_DIR")
    header_text = header.read_text()
    header_version = re.search(r'^#define GEOS_VERSION "([^"]+)"', header_text, re.MULTILINE).group(1)
    library = os.environ.get("SKARVE_GEOS_LIBRARY") or ctypes.util.find_library("geos_c")
    if not library:
        raise RuntimeError("system GEOS C library missing")
    gdal_library = os.environ.get("SKARVE_GDAL_LIBRARY") or ctypes.util.find_library("gdal")
    if not gdal_library:
        raise RuntimeError("GDAL runtime is required to verify that the optional bridge shares its GEOS instance")
    gdal = ctypes.CDLL(gdal_library)
    geos = ctypes.CDLL(library)
    geos.GEOSversion.restype = ctypes.c_char_p
    runtime_version = geos.GEOSversion().decode().split("-CAPI-")[0]
    if runtime_version != header_version:
        raise RuntimeError(f"GEOS header/runtime mismatch: {header_version} versus {runtime_version}")
    # Resolve the already loaded system object, never download a second GEOS.
    candidates = []
    for line in Path("/proc/self/maps").read_text().splitlines():
        pieces = line.split()
        if len(pieces) >= 6 and Path(pieces[-1]).name.startswith("libgeos_c.so"):
            candidates.append(Path(pieces[-1]).resolve())
    if len(set(candidates)) != 1:
        raise RuntimeError("could not resolve a unique loaded GEOS C library")
    library_path = candidates[0]
    geos_config = out / "geos-config"
    geos_config.mkdir(exist_ok=True)
    # The exact matching runtime may be installed without a development .so
    # symlink. A local imported target preserves that runtime identity.
    (geos_config / "GEOSConfig.cmake").write_text(
        f'set(GEOS_VERSION "{runtime_version}")\nset(GEOS_FOUND TRUE)\n'
        'if(NOT TARGET GEOS::geos_c)\nadd_library(GEOS::geos_c SHARED IMPORTED)\n'
        f'set_target_properties(GEOS::geos_c PROPERTIES IMPORTED_LOCATION "{library_path}" INTERFACE_INCLUDE_DIRECTORIES "{include}")\nendif()\n')
    (geos_config / "GEOSConfigVersion.cmake").write_text(
        f'set(PACKAGE_VERSION "{runtime_version}")\n'
        'if(PACKAGE_FIND_VERSION VERSION_GREATER PACKAGE_VERSION)\nset(PACKAGE_VERSION_COMPATIBLE FALSE)\nelse()\nset(PACKAGE_VERSION_COMPATIBLE TRUE)\nendif()\n')
    build_dir = out / "upstream-build"
    mappings = [(source, "exactextract"), (ROOT.parents[1], "skarve"), (out, "skarve-build"), (include, "geos-headers")]
    prefix_args = [f"-{kind}-prefix-map={path}={label}" for path, label in mappings for kind in ["ffile", "fdebug"]]
    visibility_args = ["-fvisibility=hidden", "-fvisibility-inlines-hidden"]
    prefix_flags = shlex.join(prefix_args + visibility_args)
    flags = [f"-D{key}={'ON' if value else 'OFF'}" for key, value in PIN["configuration"].items()]
    run([cmake, "-S", source, "-B", build_dir, "-DCMAKE_BUILD_TYPE=Release", f"-DGEOS_DIR={geos_config}",
         f"-DCMAKE_CXX_COMPILER={cxx}", f"-DCMAKE_CXX_FLAGS={prefix_flags}", *flags])
    run([cmake, "--build", build_dir, "--target", "exactextract", "--parallel", args.jobs])
    upstream_library = build_dir / "libexactextract.a"
    shutil.copyfile(upstream_library, out / "libexactextract.a")
    bridge_object = out / "bridge.o"
    run([cxx, "-std=c++17", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror", "-I", source / "src", "-I", include,
         *prefix_args, *visibility_args, "-c", ROOT / "bridge.cpp", "-o", bridge_object])
    run([os.environ.get("AR", "ar"), "crs", out / "libskarve_exactextract_bridge.a", bridge_object])
    if args.shared_control:
        run([cxx, "-shared", "-o", out / "libskarve_exactextract_bridge.so", bridge_object, upstream_library, library_path])
        run([cxx, "-std=c++17", "-O2", "-I", source / "src", "-I", include, *prefix_args,
             ROOT / "control.cpp", bridge_object, upstream_library, library_path, "-o", out / "skarve-ee-control"])
    receipt = {
        "upstream": PIN, "source_archive_verified": True, "source_modified": False,
        "geos_version": runtime_version, "geos_library": str(library_path), "geos_sha256": digest(library_path),
        "geos_header_sha256": digest(header), "gdal_runtime_loaded_for_geos_identity_check": gdal_library,
        "bridge_sha256": digest(ROOT / "bridge.cpp"),
        "library_sha256": digest(upstream_library), "bridge_object_sha256": digest(bridge_object),
        "build_jobs": args.jobs, "python_runtime_dependency": False,
        "cxx_visibility": "hidden; only versioned bridge C ABI has default visibility",
        "configuration": "upstream core only; external GEOS; C++17; no GDAL/CLI/Python/TBB targets"
    }
    (out / "build-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    # Rust link search needs a development name. Keep this generated symlink
    # within the unsynced target directory, pointing at the verified runtime.
    link = out / "libgeos_c.so"
    if link.exists() or link.is_symlink():
        if link.resolve() != library_path:
            raise RuntimeError("existing GEOS link points to a different runtime")
    else:
        link.symlink_to(library_path)


if __name__ == "__main__":
    main()
