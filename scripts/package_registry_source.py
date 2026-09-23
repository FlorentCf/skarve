#!/usr/bin/env python3
"""Build source-only PyPI and npm candidates from the packaged Rust crate.

These archives contain no Skarve native binary. Installation compiles the
native library on the consumer's machine and requires Rust and GDAL headers.
This command never uploads to a registry.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], *, cwd: Path) -> None:
    subprocess.run(command, cwd=cwd, check=True)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def copy(source: Path, target: Path) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)


def safe_extract(archive: Path, destination: Path) -> Path:
    with tarfile.open(archive, "r:gz") as stream:
        members = stream.getmembers()
        if not members or len(members) > 5000:
            raise RuntimeError("Unexpected Rust package member count")
        for member in members:
            path = (destination / member.name).resolve()
            if not path.is_relative_to(destination.resolve()):
                raise RuntimeError("Unsafe Rust package path")
            if not member.isfile() and not member.isdir():
                raise RuntimeError("Rust package contains a non-file member")
        stream.extractall(destination, filter="data")
    roots = list(destination.iterdir())
    if len(roots) != 1 or not (roots[0] / "Cargo.toml").is_file():
        raise RuntimeError("Rust package has an unexpected root")
    return roots[0]


def assert_source_only(archive: Path) -> None:
    with tarfile.open(archive, "r:gz") as stream:
        for member in stream:
            if not member.isfile():
                continue
            if member.name.endswith((".so", ".node", ".dll", ".dylib", ".exe", ".a", ".o", ".wasm", ".pyc")):
                raise RuntimeError(f"Native binary in source package: {member.name}")
            opened = stream.extractfile(member)
            if opened is None:
                raise RuntimeError(f"Unreadable source package member: {member.name}")
            magic = opened.read(4)
            if magic.startswith((b"\x7fELF", b"MZ", b"\x00asm")) or magic in (
                b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"
            ):
                raise RuntimeError(f"Compiled object in source package: {member.name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--allow-dirty", action="store_true", help="Local test only; mark the output as non-publishable")
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    status = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT, text=True).strip()
    if status and not args.allow_dirty:
        raise SystemExit("Commit all source changes before preparing a registry candidate")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    node_package = json.loads((ROOT / "bindings/node/package.json").read_text())
    if node_package["version"] != version:
        raise SystemExit("Rust and Node versions differ")
    with tempfile.TemporaryDirectory(prefix="skarve-registry-source-") as temporary:
        work = Path(temporary)
        target = work / "target"
        cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo")
        run([cargo, "package", "--locked", "--allow-dirty", "--no-verify", "--target-dir", str(target)], cwd=ROOT)
        crate = target / "package" / f"skarve-{version}.crate"
        if not crate.is_file():
            raise RuntimeError("Cargo did not create the expected source crate")
        source = safe_extract(crate, work / "extracted")
        assert_source_only(crate)
        copy(crate, output / crate.name)

        python = work / "python"
        python.mkdir()
        for name in ("pyproject.toml", "setup.py", "raster_engine_lab.py", "skarve_bulk.py"):
            copy(ROOT / "bindings/python" / name, python / name)
        shutil.copytree(ROOT / "bindings/python/skarve", python / "skarve", ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        for name in ("LICENSE", "NOTICE", "README.md"):
            copy(ROOT / name, python / name)
        shutil.copytree(source, python / "native_source" / source.name)
        (python / "MANIFEST.in").write_text("recursive-include native_source *\nrecursive-include skarve/_native *\n")
        run([sys.executable, "-m", "build", "--sdist", "--no-isolation", "--outdir", str(output), str(python)], cwd=ROOT)

        node = work / "node"
        node.mkdir()
        for name in ("index.mjs", "bulk.mjs", "index.d.ts", "README.md", "install.mjs"):
            copy(ROOT / "bindings/node" / name, node / name)
        for name in ("LICENSE", "NOTICE"):
            copy(ROOT / name, node / name)
        shutil.copytree(source, node / "native_source" / source.name)
        node_package["private"] = False
        node_package.pop("bundledDependencies", None)
        node_package["scripts"] = {"install": "node install.mjs"}
        node_package["files"] = ["index.mjs", "bulk.mjs", "index.d.ts", "install.mjs", "native_source", "README.md", "LICENSE", "NOTICE"]
        (node / "package.json").write_text(json.dumps(node_package, indent=2) + "\n")
        run(["npm", "pack", "--ignore-scripts", "--pack-destination", str(output)], cwd=node)

    archives = sorted(path for path in output.iterdir() if path.suffix in (".crate", ".gz", ".tgz"))
    if len(archives) != 3:
        raise RuntimeError("Expected a crate, Python sdist, and npm archive")
    for archive in archives:
        assert_source_only(archive)
    manifest = {
        "schema": "skarve_source_registry_candidate_v1",
        "version": version,
        "source_commit": head,
        "source_worktree_clean": not bool(status),
        "publication_attempted": False,
        "native_binaries_in_archives": False,
        "artifacts": [{"name": path.name, "bytes": path.stat().st_size, "sha256": digest(path)} for path in archives],
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (output / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['name']}\n" for item in manifest["artifacts"]))
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
