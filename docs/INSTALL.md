# Install Skarve

This public source-only alpha is **v0.1.1-alpha.1**, with experimental SKV v0 support. Prebuilt binaries and registry packages are not published; see [the distribution decision](../release/DISTRIBUTION.md). The frozen v0.1.0-beta.2 release remains separate. Binary qualification targets Ubuntu 24.04 Linux x86-64, Python 3.12 and Node 20. The native runtime requires system GDAL 3.8.4 (`libgdal34t64` and `gdal-data`, Ubuntu package version `3.8.4+dfsg-3ubuntu3`). Skarve's core is bundled in the wheel, npm archive and CLI archive. The npm archive also includes its Koffi dependency. These binaries make no manylinux, Windows or macOS compatibility claim. Review [external runtime licenses](RUNTIME_LICENSES.md) before redistributing a binary combination.

Linux SKV decoding additionally requires `libdeflate.so.0` with
`libdeflate_alloc_decompressor_ex`, `libdeflate_zlib_decompress_ex` and
`libdeflate_free_decompressor`. The qualified runtime is libdeflate 1.19,
Ubuntu package `libdeflate0=1.19-1build1.1`. Install it with the GDAL packages:

```sh
sudo apt-get update
sudo apt-get install libgdal34t64=3.8.4+dfsg-3ubuntu3 \
  gdal-data=3.8.4+dfsg-3ubuntu3 libdeflate0=1.19-1build1.1
```

A missing library or older library without those symbols prevents loading the
Linux core, including native-only packages. There is no hidden Linux miniz
fallback. Source builds on other platforms retain the existing miniz decoder;
that does not extend the qualified binary-platform claim. The encoder remains
flate2/miniz_oxide and the SKV representation is unchanged.

Use the immutable artifacts from the approved release and verify `SHA256SUMS`. The public repository, tag and publication channel are controlled separately from local artifact preparation. Do not substitute a moving development build for the library named in `manifest.json`.

The release's asset list determines availability. For a source-only release,
use [Build and verify from source](#build-and-verify-from-source) below. The
prebuilt wheel, Node and CLI commands apply only when those assets are included.

```sh
# In the downloaded artifact directory, with the declared system GDAL installed:
sha256sum -c SHA256SUMS
python3 -m venv skarve-env
skarve-env/bin/pip install --no-index --no-deps ./skarve_engine-0.1.1a1-py3-none-linux_x86_64.whl
skarve-env/bin/skarve --version

npm install --offline --ignore-scripts --omit=optional --no-audit --no-fund ./skarve-engine-0.1.1-alpha.1.tgz

tar -xzf skarve-0.1.1-alpha.1-linux-x86_64.tar.gz
./skarve-0.1.1-alpha.1-linux-x86_64/bin/skarve --version
```

The Python distribution is `skarve-engine`; import `skarve`. The existing `raster_engine_lab` import remains a compatibility alias. The Node package is `@skarve/engine`; its default export and `Skarve`/`RasterEngine` names refer to the same class. Normal source operations need neither NumPy nor rasterio. Python typed-buffer operations additionally need NumPy; fixture generation below uses rasterio and NumPy as test dependencies.

Leave `SKARVE_LIBRARY`, `RASTER_ENGINE_LIB`, `RASTER_ENGINE_LIBRARY`, `PYTHONPATH` and user-specific native-library search overrides unset for ordinary installed use. A missing GDAL runtime produces an explicit loader error. Install the documented system prerequisite rather than redirecting the package to an unidentified library.

## Try generated data

From the release source checkout, install the test dependencies into your environment using your ordinary dependency process:

```sh
python -m pip install numpy==2.5.3 rasterio==1.5.1
python examples/generate_fixtures.py example-data
skarve measure example-data/original.tif example-data/polygon.json \
  --crs EPSG:3857 --bands 0 --statistics sum,support,mean,min,max,count
python examples/python_source.py --fixture example-data/fixture.json --index python-index
node examples/node_source.mjs example-data/fixture.json node-index
python examples/python_bulk.py
node examples/node_bulk.mjs
```

Install Skarve into that environment first. Run Node examples from a source/consumer directory with the installed `@skarve/engine` package available to module resolution. Fixture and index destinations must be new. The generator creates mathematical data locally; it downloads no source raster. The source examples query the original file, build and reopen an optional summary-only index, then consume a native shared-scan date stack two rows at a time. See [WORKFLOWS.md](WORKFLOWS.md) for reusable API calls and policy boundaries.

## Build and verify from source

Install the Rust toolchain declared in `rust-toolchain.toml`, a C linker, `pkg-config`, GDAL development headers and libdeflate headers (`libgdal-dev libdeflate-dev` on Ubuntu 24.04). Then:

```sh
git clone --branch v0.1.1-alpha.1 https://github.com/FlorentCf/skarve.git
cd skarve
python3 scripts/build.py --test
python3 -m venv .venv
.venv/bin/python -m pip install -r scripts/packaging-requirements.txt
npm ci --ignore-scripts --prefix bindings/node
.venv/bin/python scripts/package_release.py --output-dir dist/skv-v0/native
```

Builds use locked Rust dependencies and at most two workers. Packaging accepts a clean Git checkout or the complete source archive with its verified `SOURCE_MANIFEST.json`. Commit changes before packaging a modified checkout. An output directory must be new; existing artifacts are never overwritten under the same identity. A source build on another platform is not evidence that its binaries passed the supported-platform qualification.

## Build the optional exactextract backend

The default source build above needs no exactextract dependency. To enable the
pinned C++ backend, also install CMake, a C++17 compiler and GEOS development
headers matching the GDAL/GEOS runtime. On the qualified Ubuntu environment
these are ordinary `cmake`, `g++` and `libgeos-dev` prerequisites. Then:

```sh
python3 native/exactextract/fetch.py
python3 scripts/build.py --exactextract --target target/skv-v0 --test
.venv/bin/python scripts/package_release.py \
  --binary-dir target/skv-v0/release --output-dir dist/skv-v0/exactextract
```

The fetch is explicit and validates a pinned upstream archive and source member
manifest. It is never a side effect of a native-only build. For offline source
transfer and explicit runtime/header locations, see the
[bridge build guide](../native/exactextract/README.md#building).
The optional backend requires no Python interpreter for ordinary installed
Node/CLI execution. `skarve backends` reports the installed build's availability;
the artifact manifest records the same result. Native and optional builds must
remain separate immutable artifact sets; pin their actual hashes.

## Verify installed archives

The standalone installed suite takes the artifact location explicitly and installs into a new directory outside the checkout. Pre-fetch its test wheels when network access is available, then test offline:

```sh
python3 -m pip download --only-binary=:all: --dest test-wheelhouse numpy==2.5.3 rasterio==1.5.1
python3 tests/standalone_suite.py --artifacts dist/skv-v0/native \
  --wheelhouse test-wheelhouse --scratch /tmp/skarve-consumer-skv-v0 \
  --output standalone-result.json
```

The suite removes development overrides and credentials from child environments, uses generated files and loopback HTTP only, and caps child processes. It is not itself a filesystem/network sandbox; an enclosing isolation receipt is required to claim private mounts were inaccessible. Fresh installations on one machine do not constitute a second-hardware replication. Runtime versions, artifact hashes, source checks and qualification outcomes belong to the release receipts.

For conversion and ordinary `infuse`/`carve`/`cleave` against a self-contained
snapshot, follow the [installed SKV examples](skv.md). Their destinations must
also be new. Experimental artifact hashes never replace frozen beta2 identities.

## Rust and source-only archives

Rust is a first-class consumer: see [the Rust guide](rust.md), the standalone `rust_skv` Cargo example and external-consumer qualification script. The nested `examples/rust-consumer` manifest belongs to the complete source checkout; Cargo excludes nested packages from a parent `.crate`. The packaged crate includes the standalone example and its independent-consumer test tool.

Prepare a source-only artifact from a clean Git checkout with `python scripts/package_source.py --output dist/source-candidate`. It needs no native binary and performs no publication. Run `scripts/audit_publication.py` on the source history and archive before requesting publication approval. The complete source archive includes fixtures and reproduction tools; native package assembly is a separate local/private operation with strict dependency-inventory checks.
