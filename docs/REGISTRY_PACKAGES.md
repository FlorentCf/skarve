# Source-built package installation

Skarve 0.1.1-alpha.4 prepares three source-only packages: the Rust crate,
Python sdist and npm archive. The archives contain no compiled Skarve core.
Python and npm compile the included, locked Rust source during installation.
The CLI is installed by Python or Cargo. This does not remove the need to
review the installed system libraries' terms; it avoids publishing a combined
native binary in these archives. Publication and fresh-host testing are tracked
separately from building a candidate.

Current source install target: Linux x86-64. Install a C toolchain, Rust 1.98.1
(as pinned by `rust-toolchain.toml`), `pkg-config`, GDAL development files and
libdeflate development files. On Ubuntu or Debian, the system prerequisites are:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends build-essential pkg-config libgdal-dev libdeflate-dev python3-venv python3-dev
```

Use a fresh virtual environment for Python. The following direct registry
commands apply only after the respective package/version is visible on its
registry; availability must be checked rather than assumed:

```sh
python3 -m venv skarve-env
skarve-env/bin/python -m pip install --pre skarve-engine==0.1.1a4
skarve-env/bin/skarve --version

npm install @skarve/engine@0.1.1-alpha.4

cargo install skarve --version 0.1.1-alpha.4 --locked
raster-engine --version
```

Rust consumers can depend on `skarve = "=0.1.1-alpha.4"` after crates.io
publication. Node requires version 20 or later. Python requires version 3.10
or later. This alpha does not provide a C++ installer or a Windows/macOS build.

Before registry publication, test the same installation paths from the
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
