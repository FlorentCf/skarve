# Rust consumer guide

Rust is a supported **alpha** consumer of the same native engine as Python,
Node.js/TypeScript and CLI. The qualified target is Ubuntu 24.04, Linux x86-64.
The new Rust facade does not change SKV v0, its writer or numerical kernels.

## Install from the reviewed source tree

Publication is pending owner approval; these are local source dependencies, not
instructions to download an already published crate. The package version is
`0.1.1-alpha.1`, and `publish = false` remains in Cargo.toml.

Install Rust 1.98.1 (the pinned toolchain), a C/C++ linker, pkg-config,
`libgdal-dev` and `libdeflate-dev`. The advertised native dependency baseline is
GDAL 3.8.4 and libdeflate 1.19 on Ubuntu 24.04. Source builds are not promises of
Windows/macOS support or a portable static binary. Refer to the runtime license
and distribution checklist before redistributing a compiled application.

```toml
# Your application's Cargo.toml; adjust only this reviewed checkout path.
[dependencies]
skarve = { path = "../skarve", version = "=0.1.1-alpha.1" }
serde_json = "1.0"
anyhow = "1.0"
```

The **package name is `skarve`**. Its historical **library name is
`raster_engine`**, retained for Rust and C ABI compatibility. Import the public
branded types as below; do not depend on private lab locations or load a separate
Python/Node worker.

```rust,no_run
use anyhow::Result;
use raster_engine::{CarveOptions, Skarve, skv::CompileOptions};
use serde_json::json;

fn main() -> Result<()> {
    let zone = json!({"type":"Polygon", "coordinates":[
        [[0.0,0.0],[4.0,0.0],[4.0,2.0],[0.0,2.0],[0.0,0.0]]
    ]});
    let mut skarve = Skarve::new();
    let query = CarveOptions {
        metrics: ["sum", "support", "mean", "min", "max"]
            .map(str::to_owned).to_vec(),
        ..Default::default()
    };
    {
        let mut source = skarve.infuse("example.tif")?;
        println!("{}", source.carve(&zone, &query)?);
        source.compile("new-example.skv", &CompileOptions::default())?;
    } // Source closes here. Preparation is optional.
    let mut source = skarve.infuse("new-example.skv")?;
    println!("{}", source.carve(&zone, &query)?);
    Ok(())
}
```

Coordinates must already use the source CRS. An explicit conflicting CRS is
rejected, not reprojected. Bands are zero-based exposed source bands; omitted
bands select all. `carve` preserves the strict
`native_grid_planar_fractional` default, masks/NoData, normalization and existing
summary eligibility. Results retain native field names such as `fractional_sum`
and `covered_cell_equivalents`, as well as policy and source diagnostics.

A fully runnable example generates its own 4×2×2 TIFF through GDAL, queries it,
compiles SKV, deletes only that generated TIFF, then queries the standalone SKV
and consumes a two-zone batch. It needs no Python, private data or credentials:

```sh
cargo run --locked --example rust_skv -- /tmp/skarve-rust-example-new
cargo run --manifest-path examples/rust-consumer/Cargo.toml -- \
  /tmp/skarve-rust-external-example-new
```

Both output directories must be new. The example verifies sums **36** and **72**,
TIFF/SKV result equality and complete batch consumption. Its use of GDAL to
create demonstration data is separate from ordinary consumers, which only pass
source references and polygons to Skarve.

## Genuine bounded batch execution

`cleave` returns an iterator of native result pages and borrows the session until
completion or drop. It uses the existing shared tile/geometry execution; it is
not a loop around `carve`.

```rust,no_run
# use anyhow::Result;
# use raster_engine::Skarve;
# use serde_json::json;
# fn main() -> Result<()> {
let mut skarve = Skarve::new();
let polygon = json!({"type":"Polygon", "coordinates":[
    [[0.,0.],[4.,0.],[4.,2.],[0.,2.],[0.,0.]]
]});
let job = json!({
    "zones": [{"id":"zone-1", "version":"1", "geometry":polygon}],
    "slices": [{"id":"snapshot", "spec":{"location":"example.skv"}}],
    "crs":"EPSG:3857",
    "options":{"statistics":["sum","support","mean","min","max"]},
    "budget":{"working_bytes":67108864, "geometry_bytes":8388608,
              "tile_bytes":16777216, "output_bytes":1048576}
});
for page in skarve.cleave(job, 128)? {
    let page = page?;
    println!("{}", page["rows"]); // Consume before requesting the next page.
}
# Ok(())
# }
```

The result is `serde_json::Value` intentionally: all statistics, per-band nulls,
provenance, counters and checkpoints remain available without a second lossy
result schema. JSON carries control/geometry and completed output, never complete
source pixel arrays. Caller-owned geometry and retained output pages remain the
caller's memory responsibility; the session applies the same conservative
request-control reservation and engine resource gates as the installed bindings.

## Ownership, advanced controls and cancellation

- `Source` and `Batch` mutably borrow their `Skarve` session. One operation runs
  at a time; create separately bounded sessions for application concurrency.
- Dropping a source, completed/failed batch or early-exited iterator releases
  its native registration. Explicit `.close()?` reports cleanup errors.
- `skarve.cancellation()` returns a cloneable cooperative signal. Another thread
  may call `.cancel()`. Wait for native execution to return; then call
  `skarve.reset_cancellation()` before another operation. This is not hard thread
  termination or process isolation. A source can still close after cancellation.
- `infuse_spec(json!(...))` supports the existing explicit format, band mapping,
  view, identity and HTTP budget schema. Applications must resolve source
  allowlists; this library is not an unrestricted URL endpoint.
- `source.carve_with_options(zone, json!(...))` supports advanced statistics,
  prepared index options or explicit backend/policy selection. Unknown fields
  and incompatible policies fail; the facade does not silently translate them.
- Optional exactextract requires `--features exactextract`, its separately
  verified upstream source and native build prerequisites. It is opt-in and
  numerically distinct. See the optional backend and third-party documentation.
- `skarve.request(json!(...))` exposes the ordinary protocol for ordered source
  selection, serving profiles, cache limits or other expert operations. The
  branded Rust API uses the normal admitted request boundary. Existing internal
  modules remain public for compatibility, but are not a promise of independent
  stable APIs; in particular `Session::call` is an expert lower-level interface.

## Package qualification and eventual crates.io release

Run `cargo test --locked --test rust_api`, `cargo test --locked --lib` and
`cargo test --locked --doc`. The focused API tests cover standalone SKV serving,
NoData/scaling, query policy/unknown-field rejection, no-overwrite conversion,
paged completion, early drop, error cleanup and sticky cancellation.

`scripts/test-rust-package.py --work <new-directory>` packages the crate without
publishing, extracts it outside the checkout and compiles/runs the example as an
independent Cargo consumer. `--target-dir` may select an existing build cache.
Its receipt records source archive hash, the explicit package members, Cargo/Rust
versions and consumer result. No private lab path is required by the consumer.

Cargo's explicit `include` list bounds publication contents. The optional C++
bridge build scripts and upstream license are included; the upstream downloaded
source/cache and host GDAL libraries are not bundled. Root repository and binary
release audits are additional gates. Keep `publish = false` until the owner
confirms licensing, package-name availability, approved source history and the
chosen distribution scope. No registry token or upload is needed for these tests.
