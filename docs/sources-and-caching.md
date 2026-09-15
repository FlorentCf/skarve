# Sources and caching

`infuse(source)` opens an owned reader from a local path or supported source
specification. The computational engine is independent of COG and preparation.
No universal full-raster conversion or duplication is required. CRS, band
mapping, masks, NoData and scale/offset follow the declared
[numerical/source contracts](NUMERICS.md).

An optional [SKV v0 snapshot](skv.md) can serve through these same interfaces
without the original source. Its own object generation and band mapping govern
cache identity; the original provenance is never followed during serving. Native
summary eligibility and the optional backend's numerical policy remain explicit.
SKV's directory/transport caches are bounded independently of decoded query
caching, and `use_summaries:false` provides the raw control on the same object.

Source registration returns identity, metadata and access-layout diagnostics;
`source.inspect()` refreshes its observed state. A detected mutation invalidates
the operation. Index handles remain tied to their verified source and build ID.
Do not suppress these checks to keep a retained cache warm.

Cancellation during a remote GDAL read can leave that reader in a failed state.
Await cancellation until the call drains, close the reader, then use `infuse`
with the original verified source specification to continue in the same session.
The new reader establishes source identity again; it does not bypass a changed
source or clear an error on the old reader. No partial result is returned as a
successful query. This recovery boundary applies to either calculation backend;
it does not mean every cancelled query invalidates its reader.

Python can configure the existing decoded cache with
`sk.call({'op':'configure_file_cache', 'bytes':1024*1024})`; Node uses
`await sk.configureFileCache(1024*1024)`. Retain sources across different polygons
and size cache budgets explicitly. Warm hits, misses, eviction, metadata checks
and original read counts belong in the returned diagnostics. HTTP encoded
bytes, decoded values, index metadata and output pages are separate stores.

Bounded remote access uses a source spec with `http.max_requests`,
`max_range_bytes`, `max_download_bytes` and `cache_bytes`. The server must support
ranges and the required stable-validator contract. Cache hits can avoid GET
bodies while still issuing metadata HEAD requests. There is no automatic
unbounded full download fallback. See [WORKFLOWS.md](WORKFLOWS.md#remote-source-and-bounded-reuse)
for a specification and the local generated range-server example.

An application accepting user-controlled URLs must enforce its host/address
allowlist and outbound credential boundary. Numeric backend policy does not
grant source access. Keep signed URLs and secrets out of logs and examples.
Do not confuse a tracked byte/window budget with a whole-process RSS limit.

For native SKV queries using embedded summaries, remote sources can prepare
exactly adjacent payload ranges for two required boundary leaves before reducing
them. `http.boundary_read_ahead` defaults to `true`; set it to `false` to retain
the eager read path for comparison. This capability requires summaries and an
encoded response cache of at least 4096 bytes. It does not alter the shared batch
scan, local reads, summary-disabled queries or the optional backend.

Preparation reuses the configured response cache and existing decoder. It skips
resident decoded leaves and already cached encoded demand, and admits a pair
only when all required whole responses and bounded scratch fit the existing
limits. Physical reads can occur earlier; cells and summary contributions are
consumed in their original order. Only exact adjacency is merged: no gap bytes
or fully covered interior payloads are added. A skipped pair uses ordinary reads.
Checksums, immutable-source validation, conditional requests and cancellation
remain mandatory. A failed read still invalidates the affected source handle.

Source diagnostics distinguish effective `boundary_read_ahead_enabled` capability
from actual `boundary_prefetch_admissions`. Prefetch metrics report planning,
fetches, encoded bytes and reserved scratch; planned request savings are not a
measurement of complete-query savings. Inspect actual HTTP requests/bytes and
complete consumed latency. The unchanged scratch reservation is a conservative
bound, not an allocation or process-RSS measurement.
