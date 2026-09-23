# Source-built package installation

Skarve 0.1.1-alpha.4 publishes three source-only packages: the Rust crate,
Python sdist and npm archive. The archives contain no compiled Skarve core.
Python and npm compile the included, locked Rust source during installation.
The CLI is installed by Python or Cargo. This does not remove the need to
review the installed system libraries' terms; it avoids publishing a combined
native binary in these archives. The registry release and fresh-host results
are recorded in [the release report](../release/REGISTRY_RELEASE_2026-09-24.md).

Current source install target: Linux x86-64. Install a C toolchain, Rust 1.98.1
(as pinned by `rust-toolchain.toml`), `pkg-config`, GDAL development files and
libdeflate development files. On Ubuntu or Debian, the system prerequisites are:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends build-essential pkg-config libgdal-dev libdeflate-dev python3-venv python3-dev
```

Use a fresh virtual environment for Python. Version 0.1.1-alpha.4 is available
on PyPI, npm and crates.io:

```sh
python3 -m venv skarve-env
skarve-env/bin/python -m pip install --pre skarve-engine==0.1.1a4
skarve-env/bin/skarve --version

npm install @skarve/engine@0.1.1-alpha.4

cargo install skarve --version 0.1.1-alpha.4 --locked
raster-engine --version
```

Rust consumers can depend on `skarve = "=0.1.1-alpha.4"` from crates.io.
Node requires version 20 or later. Python requires version 3.10 or later. This alpha does not provide a C++ installer or a Windows/macOS build.

For local archive testing, use the same installation paths from the
candidate archives created by `python scripts/package_registry_source.py
--output-dir <new-directory>` in a clean checkout:

```sh
skarve-env/bin/python -m pip install ./skarve_engine-0.1.1a4.tar.gz
npm install ./skarve-engine-0.1.1-alpha.4.tgz
cargo install --path ./skarve-0.1.1-alpha.4 --locked
```

The crate archive is first extracted into the named directory for that last
command. Do not use `--ignore-scripts` with the npm source archive: its install
script compiles the native library. Both Python and npm installers limit Cargo
to two build jobs and surface missing system prerequisites before compiling.
The package builder records archive hashes and refuses a dirty worktree unless
`--allow-dirty` is explicitly used for local testing. A dirty candidate must
not be published.

For PyPI, `.github/workflows/publish-pypi-source.yml` provides a manual,
tag-verified source release. It rebuilds and validates the source archive in a
job without registry credentials, then publishes only that Python sdist from a
separate `pypi` environment using PyPI Trusted Publishing. The PyPI account
uses a GitHub Trusted Publisher for project `skarve-engine`, owner
`FlorentCf`, repository `skarve`, workflow `publish-pypi-source.yml`, and
environment `pypi`. The workflow runs only when the owner dispatches it for
a release tag. npm and crates.io were published separately.
