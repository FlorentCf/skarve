# Support and execution limits

The first binary qualification is Ubuntu 24.04, Linux x86-64, system GDAL 3.8.4.
The artifact manifest records the actual linked runtime and hashes. Python uses
ctypes; Node uses bundled Koffi. GDAL remains an external prerequisite. Tests on
WSL2 are Linux tests on that physical host, not independent Windows or hardware
qualification. No manylinux, macOS, Windows, GPU or WASM claim is made.

| Area | Supported contract | Explicit boundary |
|---|---|---|
| Geometry | Straight-edge Polygon/MultiPolygon, holes, disjoint components | North-up affine with positive X and negative Y resolution; matching CRS; no repair, rotation, reprojection, resampling or antimeridian/polar handling |
| Geographic domain | Validated EPSG:4326 inputs within longitude −180…180 and latitude −85…85 | No arbitrary global spherical or wrapping query |
| Coordinates | Finite, bounded native-cell coordinates | Absolute native-cell coordinate limit 10⁹; 10,000 vertices, 128 components, 128 holes |
| Source values | Integer file types through 32 bits and float32/64; masks, raw NoData, scale/offset | File int64/uint64 and complex values rejected; valid normalized values must be finite |
| Readers | Source-format-independent `WindowSource`; local GDAL windows; bounded HTTP range access | Local GeoTIFF and NetCDF date cases exercised; COG is the principal qualified remote case, not a required computational format; not every GDAL driver/codec is qualified |
| Preparation | Optional original-source summaries and explicit normalized value representation | No universal conversion; original source supplies boundary values; insufficient summaries fall back to raw data |
| Batch | Shared tile-driven many-polygon/many-slice execution, lazy compatible readers, bounded result pages | Aligned grids and explicit budgets; one internal worker; no unbounded dense geometry matrix |
| Optional exactextract | Pinned C++ fractional backend over Skarve-normalized source windows | Exactly equal source grids/CRS; five common statistics; finite complete-job typed staging before pages; no resume/derived expressions/native index |
| Cache | Explicit byte-bounded encoded/decoded caches, eviction and source checks | A warm decoded cache can still issue metadata validation; compressed bytes are not resident decoded pixels |
| Lifetime | Retained readers/index handles, explicit close, cooperative cancellation | One active call per engine; no hidden unbounded request queue; cancellation observes bounded operation boundaries |
| Identity | Local stat/sidecar snapshots, declared content identity, remote stable-object/ETag contracts | Stat is not a cryptographic guarantee; provider validators and caller-supplied identities retain their trust boundaries |
| Presentation | Full results or explicitly versioned numeric batch pages | Native six-statistic and delegated five-statistic descriptors differ; preserve provenance; timing subspans can overlap |
| Typed values | Up to 64 bands, 4,096 windows, 128 MiB combined payload/control per call | 268,435,456 contributions; bindings reserve at most 512 MiB aggregate snapshots per process; caller/native/cache memory is additional |

The session's 2 GiB limit accounts for tracked native allocations and reservations.
Decoded caches are explicitly configured, at most 128 MiB per session. Host-language
objects, codec/GDAL caches, allocator overhead and other processes also consume
memory. Deploy with a separate process/container ceiling and timeout appropriate
to your workload. A request-level bound is not a complete RSS guarantee.

Direct original-source execution is the ordinary path when no index is supplied.
Fixed-window batch execution is the conservative default. `window_policy:
source_layout`, compact/shared geometry layouts, cumulative fields, hierarchy and
joint range planning remain explicit expert options with workload-dependent
costs. Building summaries for one small query, decoding sparse encoded windows,
or overflowing a cache can be slower than competent existing implementations.
The release retains measured losses rather than claiming an optimal dispatcher.

The optional embedded exactextract backend serializes calls across Skarve
sessions because the pinned upstream feature implementation shares a GEOS
context. Cancellation checks cover admission, reads and progress boundaries;
an active geometry/reduction phase may finish before the call drains. It offers
no hard termination deadline or process-isolation guarantee. See
[Backends](backends.md) for the declared calculation and execution envelope.

Callers must enforce application URL allowlists, authentication and filesystem
permissions. Remote examples use local generated data and placeholder-only
authorization. Never pass an arbitrary unauthenticated web request's URL or
native-library path directly to an engine with privileged access.
