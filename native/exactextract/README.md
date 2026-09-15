# Optional exactextract bridge

Skarve's `exactextract` Cargo feature builds the unmodified exactextract 0.3.0
C++ library and this small synchronous C ABI adapter. Ordinary builds have no
exactextract requirement. Node, Python and CLI callers use the installed engine;
the optional backend never invokes a Python worker.

The pinned source commit is `94f5882ad6904d9d44d9199164d671fa48dc78eb`.
[upstream.json](upstream.json) records the exact archive size, SHA256 and build
configuration. [LICENSE.exactextract](LICENSE.exactextract) is the unchanged
upstream Apache-2.0 license. Skarve's bridge is independently authored; the
upstream fractional coverage and aggregation algorithms remain attributed to
ISciences/exactextract. No source patches are applied upstream.

## Building

The qualified optional build is Linux with C++17, CMake >=3.15, Python 3 for
build orchestration only, and GEOS development headers matching the GDAL runtime.
The build compiles the upstream core only: no CLI, Python bindings, GDAL reader,
TBB, documentation or upstream test targets. It loads the configured GDAL and
GEOS runtimes during build validation and rejects multiple loaded GEOS C objects
or a header/runtime version mismatch. The external runtime obligations described
in [RUNTIME_LICENSES.md](../../docs/RUNTIME_LICENSES.md) still apply.

```sh
# Ordinary project prerequisites, e.g. on Ubuntu:
# g++, cmake, libgdal-dev and matching libgeos-dev
python3 native/exactextract/fetch.py
cargo build --release --locked -j2 --features exactextract
```

Fetching is an explicit command, never a side effect of the default native build.
The source archive goes to `~/.cache/skarve-exactextract`; every pinned member is
verified before each optional build, and modified/extra source files are rejected.
For an offline build, transfer the archive named in `upstream.json` and run
`python3 native/exactextract/fetch.py --archive /path/to/pinned.tar.gz`.
`SKARVE_EE_CACHE` chooses another cache. `SKARVE_CMAKE`, `CXX`,
`SKARVE_GEOS_INCLUDE_DIR`, `SKARVE_GEOS_LIBRARY` and `SKARVE_GDAL_LIBRARY`
allow explicit tool/header/runtime locations. The build never installs or
replaces system libraries. `cargo -j2` invokes one upstream compiler worker to
leave room for another Cargo compilation; a standalone helper can use two.

## ABI and lifetime

[skarve_exactextract.h](skarve_exactextract.h) is internal ABI version 2. Request memory,
geometry WKB and output allocations belong to the caller until synchronous return.
Each read callback returns a leased, contiguous, normalized `double` window and
optional byte validity. Values are already interpreted by Skarve: mask/NoData,
scale/offset and binary64 normalization are source decisions. The upstream
calculation is over those values; it is not a promise to match every Rasterio
file adapter's dtype/scale behavior. No extra C++ pixel buffer is created.
Leases remain alive for as long as upstream needs them; the release callback
returns allocation ownership to Rust. Rust callbacks must catch panics and never
reacquire their active session lock. C++ exceptions never cross the ABI.

Every source in a call must have the same explicit CRS and exactly equal native
grid. Skarve checks CRS before entering C++; the bridge checks all grid values.
No resampling or overview selection occurs. Each valid Polygon/MultiPolygon WKB
is parsed once by the bridge. The upstream raster-sequential processor clones
features internally; this is upstream allocation, not a zero-copy geometry claim.

All features and selected bands enter one feature-sequential or raster-sequential
upstream processor. A numeric `Feature` setter writes straight into the final
caller-staged array, avoiding a result property tree and dense-conversion pass.
The request carries a statistics mask. Only selected upstream operations plus
internal support are registered: an unrequested sum overflow cannot invalidate
a valid minimum-only request. The fixed result order remains sum, **fractional
support**, mean, min, max. Unrequested slots have zero placeholders and
`defined=0`, except support, which is retained internally to determine status.
This internal version does not change the public `re_*` functions or bulk ABI 1.
It supplies no
integer valid-cell count. Zero support has defined zero sum/support and undefined
mean/extrema. Undefined entries carry a zero placeholder plus `defined=0`.
Nonfinite valid input and binary64 result overflow are rejected. Any nonzero
status invalidates the **entire** staged output, even if some cells were written.

## Resources, cancellation and concurrency

Input and live-window limits are checked before reads. `max_cells` bounds the
upstream subdivision size; `max_live_window_bytes` bounds callback leases using
nine bytes per cell. Neither is a process-RSS limit: GEOS geometries, upstream
registries, feature clones and allocator overhead remain separate. The caller's
source/cache/output budgets remain in force and can impose lower admission caps.

Cancellation is cooperative before/after reads, at feature/progress/output
boundaries, and while waiting for execution. An upstream geometry operation or
reduction in progress cannot be interrupted immediately. Requests requiring hard
process isolation or hard deadline termination are ineligible for this embedded
backend. Request memory cannot be released before the call returns.

Pinned upstream `MapFeature` uses one static GEOS context. All embedded calls
therefore serialize behind a mutex. Waiters poll cancellation every two
milliseconds. Reentrant calls fail rather than deadlock. This includes unrelated
Skarve sessions; no parallel embedded throughput claim is made.

Metrics distinguish callback time, upstream `process()` time and total bridge
time. Callback time is nested within upstream time; upstream time is nested within
total. Never sum these overlapping spans. Total includes queueing, validation,
geometry conversion, operation setup and cleanup. Read bytes count interpreted
values/masks, not physical disk or network traffic.

## Independent C++ control and focused tests

```sh
python3 native/exactextract/build.py --out target/exactextract-control --shared-control --jobs 2
python3 native/exactextract/test_bridge.py target/exactextract-control/libskarve_exactextract_bridge.so -v
target/exactextract-control/skarve-ee-control upstream job.bin 5
target/exactextract-control/skarve-ee-control bridge job.bin 5
```

The control's binary input is little-endian: magic `SKAREE01`, six uint64 values
`strategy,width,height,bands,zones,max_cells`, six binary64 values
`xmin,ymin,xmax,ymax,dx,dy`, then each band's `width*height` binary64 values followed
by that many validity bytes. Finally, each zone is a uint64 WKB byte length and
its WKB. Strategy is 0 for feature-sequential and 1 for raster-sequential.

`upstream` uses an independently implemented array `RasterSource`, the natural
upstream processor and a `MapWriter`, followed by one dense numeric conversion.
`bridge` supplies the same arrays through the C ABI with explicitly charged
window-copy callbacks. Both link the exact archive used by Skarve. Neither
control includes original-file opening or decoding. JSON output includes source
snapshot loading separately from each complete operation and its nested upstream
span. Result JSON serialization is outside those spans. The installed source
benchmark must account for its additional source/cache/guard responsibilities.
