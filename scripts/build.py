#!/usr/bin/env python3
"""Build Skarve with locked dependencies, two workers and remapped source paths."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]

def build_env(target=None):
    env = os.environ.copy()
    env['PATH'] = str(Path.home() / '.cargo/bin') + os.pathsep + env.get('PATH', '')
    env.setdefault('CARGO_BUILD_JOBS', '2')
    if target:
        env['CARGO_TARGET_DIR'] = str(Path(target).resolve())
    env['RUSTFLAGS'] = f'--remap-path-prefix={ROOT}=skarve --remap-path-prefix={Path.home() / ".cargo/registry"}=registry'
    if not shutil.which('gdal-config') and not env.get('GDAL_LIB_DIR'):
        # Compatibility with the qualified Ubuntu runtime-only installation.
        # Ordinary source builds should install libgdal-dev instead.
        system = Path('/usr/lib/x86_64-linux-gnu/libgdal.so.34')
        if not system.is_file():
            raise SystemExit('GDAL is missing. Install libgdal-dev (Ubuntu 24.04), then retry.')
        linkdir = Path(env.get('CARGO_TARGET_DIR', ROOT / 'target')) / 'gdal-link'
        linkdir.mkdir(parents=True, exist_ok=True)
        link = linkdir / 'libgdal.so'
        if not link.exists():
            link.symlink_to(system)
        env.update(GDAL_LIB_DIR=str(linkdir.resolve()), GDAL_VERSION='3.8.4')
    if not shutil.which('cargo', path=env['PATH']):
        raise SystemExit('Rust cargo is missing. Install the toolchain in rust-toolchain.toml, then retry.')
    return env

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--target', type=Path)
    p.add_argument('--test', action='store_true')
    p.add_argument('--exactextract', action='store_true', help='Enable the pinned optional C++ backend; fetch its verified source explicitly with native/exactextract/fetch.py first.')
    a = p.parse_args()
    env = build_env(a.target)
    features = ['--features', 'exactextract'] if a.exactextract else []
    subprocess.run(['cargo', 'build', '--locked', '--release', '-j', '2', *features], cwd=ROOT, env=env, check=True)
    if a.test:
        subprocess.run(['cargo', 'fmt', '--all', '--check'], cwd=ROOT, env=env, check=True)
        subprocess.run(['cargo', 'test', '--locked', '-j', '2', *features], cwd=ROOT, env=env, check=True)

if __name__ == '__main__':
    main()
