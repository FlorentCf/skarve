#!/usr/bin/env python3
"""Explicitly fetch hash-pinned upstream source; ordinary native builds do not fetch."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parent
PIN = json.loads((ROOT / "upstream.json").read_text())


def cache_root():
    return Path(os.environ.get("SKARVE_EE_CACHE", Path.home() / ".cache" / "skarve-exactextract"))


def unpack(archive, destination):
    data = archive.read_bytes()
    if len(data) != PIN["archive_bytes"] or hashlib.sha256(data).hexdigest() != PIN["archive_sha256"]:
        raise RuntimeError("exactextract upstream archive hash mismatch")
    if destination.is_symlink():
        raise RuntimeError("upstream source cache must not be a symlink")
    destination.mkdir(parents=True, exist_ok=True)
    expected = set()
    with tarfile.open(archive) as source:
        members = source.getmembers()
        prefix = f"exactextract-{PIN['commit']}/"
        for member in members:
            if member.name == prefix[:-1] and member.isdir():
                continue
            if not member.name.startswith(prefix) or member.issym() or member.islnk() or not (member.isfile() or member.isdir()):
                raise RuntimeError("unexpected upstream archive member")
            relative = Path(member.name[len(prefix):])
            if relative.is_absolute() or ".." in relative.parts:
                raise RuntimeError("unsafe upstream archive path")
            if member.isfile():
                expected.add(relative.as_posix())
                target = destination / relative
                payload = source.extractfile(member).read()
                if target.is_symlink() or any(parent.is_symlink() for parent in target.parents if parent != destination.parent):
                    raise RuntimeError("upstream source cache contains a symlink")
                if target.exists() and target.read_bytes() != payload:
                    raise RuntimeError(f"refusing to overwrite modified upstream source: {relative}")
                target.parent.mkdir(parents=True, exist_ok=True)
                if not target.exists():
                    target.write_bytes(payload)
    for path in destination.rglob("*"):
        if path.is_symlink() or (path.is_file() and path.relative_to(destination).as_posix() not in expected):
            raise RuntimeError("upstream source cache contains an unpinned extra file or symlink")
    return destination


def verified_source():
    cache = cache_root()
    archive = cache / f"exactextract-{PIN['commit']}.tar.gz"
    if not archive.is_file():
        raise RuntimeError("pinned optional exactextract source missing; explicitly run python3 native/exactextract/fetch.py")
    # Verify every source file against the pinned archive before each build.
    return unpack(archive, cache / f"exactextract-{PIN['commit']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, help="Use an already downloaded pinned archive, without network")
    args = parser.parse_args()
    cache = cache_root()
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f"exactextract-{PIN['commit']}.tar.gz"
    if not archive.exists():
        if args.archive:
            data = args.archive.read_bytes()
        else:
            with urllib.request.urlopen(PIN["url"], timeout=60) as response:
                data = response.read(PIN["archive_bytes"] + 1)
        if len(data) != PIN["archive_bytes"] or hashlib.sha256(data).hexdigest() != PIN["archive_sha256"]:
            raise RuntimeError("exactextract download failed pinned size/hash validation")
        with archive.open("xb") as stream:
            stream.write(data)
    print(verified_source())


if __name__ == "__main__":
    main()
