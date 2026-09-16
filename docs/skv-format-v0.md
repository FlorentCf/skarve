# SKV v0 experimental serving format

Status: working experiment, unstable. SKV is an optional compiled snapshot,
not a replacement raster model or an assertion of a new aggregation algorithm.
The original source is provenance only; serving never follows its location.

## Logical contract and limits

North-up regular native grids retain their affine transform, CRS and pixel
convention. Up to 64 metadata bands retain original band order, description,
units, scalar type, scale/offset and raw NoData represented as f64 bits. Supported
samples are GDAL Byte, Int8, UInt16, Int16, UInt32, Int32, Float32 and Float64 where
the installed GDAL supports them. Samples are stored little-endian without a
numeric cast, including invalid floating payloads and signed zero. Mask bytes
retain their original 0..255 values. The NoData bits are GDAL's exposed metadata,
not a promise to reconstruct arbitrary original TIFF tag bytes. Complex and
64-bit integer samples reject. Query normalization reuses the existing reader
interpretation; valid nonfinite results reject or are excluded under that same
contract, never silently repaired.

All dimensions/counts/offsets use checked arithmetic. Grid limits remain the
engine's limits; a v0 object is at most 8 GiB and 131,072 typed leaf chunks
(spatial chunks multiplied by stored bands). The latter bounds visited-range
bookkeeping and explicit verification work; it is an experimental implementation
limit. Chunk edges are 64, 128 or 256 pixels;
the summary hierarchy initially uses the same native chunks. Window calls remain
up to64 selected bands when the source's complete read bound fits the unchanged
query allowance. SKV advertises that capability; ordinary readers retain20-band
reads and orchestration shrinks groups when necessary for36/40-band jobs.
Metadata JSON is at most 64 KiB decompressed and must fit the bounded bootstrap
after zlib encoding. Units/descriptions/counts have additional reader bounds.

## Bootstrap: 16,384 bytes

All integers are unsigned little-endian. Unknown flags, versions, codecs,
nonzero reserved bytes, impossible shapes or inconsistent derived lengths reject.

| Offset | Bytes | Meaning |
|---:|---:|---|
| 0 | 8 | Magic `SKVRAST\0` |
| 8 | 4 | Major format version, 0 |
| 12 | 4 | Feature bits: bit0 stored summaries, bit1 `byte_delta_v1` predictor, bit2 explicit source-view provenance, bit3 `row_group_v1` payloads; all others reserved |
| 16 | 4 | Bootstrap bytes, 16384 |
| 20 | 4 | Metadata codec, 1 = zlib DEFLATE |
| 24 | 4 | Encoded metadata length |
| 28 | 4 | Decoded metadata length, <=65536 |
| 32 | 8 | Exact complete object length |
| 40 | 8 | Directory offset, 16384 |
| 48 | 8 | Directory page count |
| 56 | 8 | First payload byte; equals directory end |
| 64 | variable | Encoded metadata, then zero padding |
| 16352 | 32 | BLAKE3 of preceding bootstrap bytes |

Metadata records the grid/raw metadata, source provenance without a locator,
unique build generation, logical-data receipt, layout/chunk edge/band group,
hierarchy levels, numerical schema and directory digest. Its own serving identity
is derived from this verified bootstrap and pinned object generation. An optional
explicit whole-file identity verification is charged at open; it is not required
for every selective query. Default local file stat checks and remote strong ETag
conditional ranges retain the same authority model as ordinary source access.
The default cache identity also binds the serving generation and selected band
mapping. An explicit trusted/verified content manifest can instead establish a
portable identity. The bootstrap checksum is not a Merkle proof for unread pages.

For a known remote SKV (`.skv` suffix or explicit format selector), registration
uses one `Range: bytes=0-16383` GET and consumes that validated bootstrap once.
The response must be206 with a strong ETag, exact Content-Range and Content-Length,
and identity content encoding. A declared expected ETag is sent as If-Match from
the first request. A source shorter than the range may return its exact clipped
range, but an incomplete SKV bootstrap rejects. Range/download caps below16 KiB
reject before this registration request; no HEAD or whole-file fallback occurs.
The initial body is charged once and kept outside the optional response cache.
Both query-entry and query-exit generation checks remain, as do conditional
subsequent ranges and fail-closed invalidation. For URLs without a query string,
these guards require conditional HEAD support. Query-bearing URLs retain the
existing uncached conditional GET probe for byte0, charged separately at each
guard. A successful bootstrap GET alone does not establish compatibility with
a provider's presigned-URL policy; that endpoint must permit the subsequent
conditional ranges and both generation probes. Real-provider confirmation is
separate from controlled HTTP tests. Unknown suffix sniffing retains its
separately accounted generic registration/probe path. Other formats and local
files retain their existing source access.

The decompressed metadata is a UTF-8 JSON object with these fields. Unsigned bit
fields must be parsed as exact integers, not rounded through a binary64 JSON
number representation.

| Field | Meaning |
|---|---|
| `version` | Integer 0 |
| `grid` | Native `width`, `height`, six-element affine `transform`, and `crs` |
| `raw_metadata` | `bands`, `pixel_convention`, `source_band_count`, and optional `source_overview` |
| `chunk_edge`, `band_group` | Stored edge and physical grouping maximum |
| `payload_layout` | `band` or `row_group_v1`; omitted means `band` |
| `codec`, `compression_level` | Requested `none`/`deflate` and level 0..9; each record declares its actual codec |
| `predictor` | `none` or `byte_delta_v1`; omitted means `none` |
| `summaries` | Whether complete stored summaries are available |
| `hierarchy` | `tile_edge` and leaf-first `levels`, each `[columns, rows, first_node_id]` |
| `numerical_schema` | `native_grid_planar_compensated_v1` |
| `build_id` | 64 hexadecimal characters identifying this build |
| `original_source_id`, `original_identity` | Original source provenance; never a serving dependency |
| `logical_digest`, `directory_digest` | 64-character hexadecimal BLAKE3 digests with scopes defined below |
| `summary_disabled_reason` | Optional bounded explanation when summaries cannot be represented |

Each band has `scalar_type` (`byte`, `int8`, `u_int16`, `int16`, `u_int32`,
`int32`, `float32`, or `float64`), nullable `nodata_f64_bits`, exact unsigned
`scale_f64_bits` and `offset_f64_bits`, nullable `unit`, `mask_flags`,
`original_band_index`, and `description`. Scale/offset bits must represent finite
binary64 values. NoData bits may represent a NaN. `mask_flags` retains GDAL's
four low bits; actual mask bytes are stored separately even for all-valid masks.
Original indices must be distinct and below `source_band_count` (at most 65,536).
Units are at most 1,024 UTF-8 bytes, descriptions 4,096 bytes. Pixel convention is
`Area`, `Point`, or `unspecified`. A present `source_overview` is the explicit
zero-based existing source view, less than 64; its grid is already the stored
grid. SKV serving never chooses or generates an overview.

Bootstrap flags must exactly equal those derived from the metadata. Header and
hierarchy objects reject unknown fields. Missing optional layout/predictor/view
fields preserve earlier v0 interpretation; a grouped object cannot omit bit3 or
its layout name. This is an unstable experimental extension, not an assurance
that a reader predating a required feature can read it.

## Paged directory and summary states

Each page is 8,240 bytes: 16-byte prefix, 64 records of 128 bytes, then 32-byte
BLAKE3 checksum. Prefix: `SKVP` magic, u16 version0, u16 occupied count, u64 page id.
Final-page unused records must be zero. The page address is directly computable
from its id; no whole-directory bootstrap is required.

On a remote cache miss, the reader requests an aligned slab of at most8 adjacent
directory pages, clipped to the actual directory end and the HTTP range cap.
Every fetched page checksum/identity is validated before any page enters the
existing64-page LRU. Local access still reads one page. This intentionally trades
some metadata overread for fewer request dependencies; `directory_bytes` includes
all fetched bytes, with extra prefetched pages and cache evictions reported.
It fetches no raw payload bytes. Summary-read admission reserves the slab, a
page copy, transport scratch and record/state temporaries separately from retained
cache storage. A range cap smaller than one page rejects as before.

Native hierarchy nodes are row-major per level, leaves first, then 2x2 parents
until one root. Node/band record id is `node_id * band_count + band_id`.

| Record offset | Bytes | Meaning |
|---:|---:|---|
| 0 | 8 | Encoded payload offset, zero for parent |
| 8 | 4 | Encoded payload bytes |
| 12 | 4 | Decoded sample-plus-mask bytes; entire packet for grouped leaves |
| 16 | 8 | Typed sample bytes for this one band's true tile shape |
| 24 | 4 | Payload codec: 0 uncompressed, 1 zlib DEFLATE |
| 28 | 4 | Flags: bit0 leaf, bit1 summary present |
| 32 | 32 | Encoded payload checksum with the layout-specific domain below (zero for parent) |
| 64 | 40 | Sum main/correction f64, valid count u64, min/max f64 |
| 104 | 8 | Expected record id |
| 112 | 8 | Canonical leader record id for grouped leaf; otherwise zero |
| 120 | 4 | Group member count for grouped leaf; otherwise zero |
| 124 | 4 | Reserved zero |

Empty summary states encode zero sums/count/extrema. Nonempty states pass the
existing finite/count/extrema consistency safeguards. The schema is
`native_grid_planar_compensated_v1`; merge operations use the existing `Sum` and
`Acc` implementation. Parent summaries merge children in the existing native
hierarchy order. Mean derives from numerator/support, never averaged child means.
Summary eligibility is decided by the engine; HM ordered folds, incompatible
weights/expressions and upstream exactextract do not silently consume them.
If an otherwise valid typed dataset produces an unrepresentable compensated
summary, compilation preserves the raw values and disables summary use for the
whole object, with an explicit reason. Unrequested summary overflow cannot prevent
the lossless snapshot. Raw-only builds currently retain the same fixed directory
record space, so disabling summaries is an algorithm control, not a space-saving
layout claim.

## Samples, masks and physical order

A default `payload_layout="band"` leaf contains row-major typed samples followed
by row-major mask bytes, using its true edge-chunk shape. Each band/chunk is
independently compressed: selecting one band does not inflate all 40.
`band_group` controls physical order:
groups of adjacent bands, then spatial tiles, then bands inside the group. This
permits bounded per-band/small-group/all-band layout comparisons without changing
the mathematical dataset. Compression level is recorded; uncompressed payloads
are a required control. No generated overview, resampling, lossy prediction or query answers
are stored. The codec is standard zlib-wrapped DEFLATE. Encoding uses pinned flate2/miniz_oxide.

The optional `predictor: "byte_delta_v1"` is a reversible transform before the
payload codec. The default `none` preserves the initial encoding. For a chunk
with actual width `w`, height `h`, scalar byte width `s`, and `cells = w*h`, the
transformed sample at `b*cells + y*w + x` is the original little-endian byte at
`(y*w+x)*s+b`, minus the preceding sample's byte in the same row and byte plane,
wrapping modulo256. The preceding byte at column0 is0. Decoding takes a wrapping
prefix sum independently in each row/plane, then interleaves those byte planes.
The mask suffix is unchanged. No floating arithmetic or normalization occurs;
signed zero, NaN payloads and invalid samples retain their exact stored bits.

The header predictor field is omitted for `none`; a missing field means `none`,
so initial v0 objects remain readable. Enabled objects require bootstrap bit1
and the exact predictor name to agree. Older readers reject the new required
feature bit. Record shapes, codecs, offsets and summary states are unchanged.
Checksums cover encoded bytes; the logical receipt covers restored original
sample/mask bytes. The transform is also allowed with the uncompressed codec as
a correctness control. One full sample scratch buffer is at most524,288B and
fits the existing8MiB read allowance alongside one combined range and one decoded
chunk. Predictor time and scratch are reported separately. This established
preprocessing technique is evaluated as a storage/transfer correction; enabling
it does not itself establish a speed claim or change numerical policy.

### Optional grouped rows

`payload_layout="row_group_v1"` shares one bounded payload across contiguous
bands of the same scalar byte width. Different scalar types of that width are
allowed; their bits and individual interpretation metadata remain distinct.
For each group, starting at the first remaining stored band, set

```text
capacity = min(band_group, floor(3276800 / (chunk_edge² * (scalar_bytes + 1))))
```

Take the longest contiguous equal-width prefix of at most `capacity` bands, then
repeat at the next band. Membership uses the full configured tile shape even at
clipped edges. This makes the partition independently derivable from the header.
For 128-pixel tiles, 40 Float32 bands form one 3,276,800-byte full packet;
64 Float32 bands split 40+24; 64 Float64 bands split 22+22+20. For 256-pixel tiles,
at most 10 Float32 or 5 Float64 bands fit per group. A smaller `band_group` can
split further. The physical order is group, spatial tile, then one shared packet.

Let the actual tile have `w*h` cells, scalar width `s`, and `g` group members.
The decoded packet has `w*h*(s+1)*g` bytes. Sample bytes come first, in
`row -> little-endian byte plane -> column -> member` order. For member `m`:

```text
sample_position = ((y*s + plane)*w + x)*g + m
mask_position   = w*h*s*g + (y*w + x)*g + m
```

With predictor `none`, the sample byte is the original typed sample's
little-endian byte. With `byte_delta_v1`, it is that same byte minus the previous
column's original byte of the same member and plane, modulo 256; column zero
uses a zero predecessor. Prediction never crosses rows, planes or bands. This
reorders the existing SKV byte-plane predictor; it does not use TIFF predictor3's
different byte order/carry convention. Mask bytes are neither predicted nor
normalized. The native compiler may retain an uncompressed packet (record codec0)
if DEFLATE would exceed its decoded length. The header still records the requested
codec. Both encoded and decoded grouped packets are at most 3,276,800 bytes.

For a group beginning at stored band `first` of node `n`, its leader is
`n*band_count + first`. All member records share offsets 0..28, checksum 32..64,
leader/count 112..124, and zero padding 124..128. Record id and summary state
remain per-band. Parent records have no payload or group fields. A reader checks
the deterministic partition and compares a requested nonleader's descriptor with
its canonical leader before reading raw bytes; matching shape alone is not enough.
Only canonical leaders enter the visited physical interval registry. Exact alias
groups are permitted; distinct canonical groups sharing or overlapping an
interval reject, even if each local checksum is coherent.

Payload checksum inputs are the following exact concatenations (`LE32` and `LE64`
denote little-endian integers, and `record[12:24]` is a 12-byte slice):

```text
band:         BLAKE3(LE64(record_id) || encoded_payload)
row_group_v1: BLAKE3(b"SKV-row-group-v1\0" || LE64(leader) || LE32(g)
                     || record[12:24] || encoded_payload)
```

The grouped compiler hashes each packet once and copies that checksum into its
aliases. A selected group is decoded once per raw window call, even when its
requested members are permuted. A one-band request must still fetch and decode
that band's whole packet. Separate raw calls, including per-band upstream backend
calls, can decode the same group again; there is no retained decoded-group cache.
These costs belong in sparse-band and backend comparisons.

Each requested page and payload is checked independently. Encoded and decoded
lengths must match checked shapes and budgets; trailing codec data rejects.
Visited payload intervals are checked for overlap; full verification checks all
intervals, directory digest and logical receipt. Integrity is corruption detection,
not authentication against coherent replacement of the entire object.
`logical_digest` is BLAKE3 over `(record_id as u64 little-endian, original typed
sample bytes followed by original mask bytes)`
for each leaf record in directory order. It is a digest of the chunk-serialized
logical bytes, so changing chunk edge changes this digest. It is not a
grid-independent identity of the raster. For fixed chunk edge and stored band
mapping, changing codec, predictor or payload layout preserves this digest.
`directory_digest` hashes every complete
directory page in page order, including page checksums and padding.

## Construction, verification and serving

The compiler receives a dataset and options, never final polygons. It streams
bounded raw windows into a task-owned temporary file with a temporary on-disk
directory, creates reviewed summaries, assembles a complete file, validates its
structure and rechecks the pinned input generation before atomic no-overwrite
publication. Before that commit point, cancellation/error removes only task-owned
incomplete files and does not leave a completed final object. Cancellation arriving
after the atomic commit does not roll back a completed object. A terminated process
can leave clearly named `.incomplete`/`.records` scratch files; serving does not
discover them as completed output. Full immutable rebuild is the update
model. Scratch reads/writes, source reads, compression/summary time, final bytes,
and original-plus-derived retention are included in conversion receipts.
The initial compiler reads one typed band per chunk call, even when physical
`band_group` is larger. It never claims that grouping already avoids repeated
decoding of a pixel-interleaved input. Compilation independently decodes its
completed temporary object, validates every state against the raw values/children,
then performs a charged full-file SHA256 pass. Atomic no-replace finalization uses
a same-filesystem hard link; unsupported filesystems fail closed. Parent-directory
sync success is reported separately. The default compiler allowance is64MiB,
with a32MiB minimum working bound; some physical TIFF layouts require more.

An explicit SKV source or `.skv` suffix routes to a reader that validates the
magic. Ordinary `.tif`/`.tiff` paths go directly to GDAL without an extra sniff
request. Unknown suffixes use a bounded eight-byte magic probe; its bytes and
requests remain charged. A renamed SKV with a TIFF suffix requires explicit
`format="skv"`. Local and HTTP sources use the
ordinary `WindowSource` interface; raw reads remain valid without summaries.
`compile` is explicit and `ward` retains its existing optional preparation role.
Remote reads use the existing bounded conditional range transport and never read
the original provenance location. Whole-job failure publishes no successful
partial answer. Cancellation remains cooperative with declared transport limits.
The reader retains at most 64 directory pages and reserves visited payload
intervals from the validated physical payload count (at most 131,072), rather
than a fixed global maximum. It records at most 4,096 typed trace events. There is no decoded payload cache in
this first version. The configured transport cache remains bounded separately;
the reader's aggregate retained bound includes both. A source handle admits at
most 65,536 lifetime logical reads and the existing configured HTTP request/byte
limits. Explicit query renewal resets transport allowances, not this separate
SKV lifetime limit. Full local verification uses a finite record/page-derived
allowance and restores the serving limit afterward, including on failure.
Remote verification retains ordinary limits.

The capacity increase changes no v0 bytes or interpretation: smaller existing
objects remain readable. Older readers can reject objects above their 48,000
typed-leaf implementation limit. An unchanged format version does not imply
that every older runtime admits the larger capacity.

The reader sorts at most 64 selected records within each requested spatial tile
and keeps a bounded queue of at most 256 records across adjacent tiles. It can
combine exactly adjacent required payload intervals, including horizontal tile
neighbors, and restores the caller's band order when copying decoded values.
Ranges never include gaps, unrequested packets or spatial tiles already answered
by summaries. Group aliases are deduplicated before transport and never split
between queue flushes; unrequested member bands inside a selected packet remain
the explicit grouped-layout overread described above. The byte cap is the smaller
of the configured HTTP limit and 4 MiB for independent bands, or 3,276,800 bytes
for grouped rows. Every constituent packet retains its own checksum and exact
decoded-length check. A single encoded packet exceeding the HTTP cap rejects
before a raw read; it is not silently split or exempted from that cap.

The caller's final typed sample/mask window is reserved separately. A grouped
read keeps one combined encoded range and one decoded packet. Its fused restore
writes directly into that admitted output, without allocating a complete member
or an inverse-predictor buffer. It visits only intersecting rows. Predicted data
still requires each requested row's prefix from column zero to the requested
right edge, including masked prefix cells; it writes only the intersection.
No floating-point interpretation occurs in this operation. The original complete
member extraction/inverse path remains in full verification as a separate control.
The unchanged conservative read allowance is:

| Simultaneously live buffer | Maximum bytes |
|---|---:|
| Coalesced encoded range | 3,276,800 |
| One decoded packet | 3,276,800 |
| Historical restored-band reserve; unused by fused window restore | 589,824 |
| Historical inverse-predictor reserve; unused by fused window restore | 524,288 |
| Codec, HTTP, record queue and structural reserve | 524,288 |
| **Total within the existing 8 MiB allowance** | **8,192,000 / 8,388,608** |

The queue and current tile records together use at most 65,536 bytes, included
in the last reserve. Directory LRU and compressed transport cache are bounded
retained capacities reserved separately; directory fetching does not overlap the
packet restore phase. Normalized output, geometry, reducers and batch output have
their own existing admission bounds. Grouping increases the compiler's declared
working bound by up to two packet capacities plus 1 MiB, and cannot bypass the
requested compiler allowance. The update cost remains a complete new snapshot.

Diagnostics count actual ranges, encoded/decoded bytes, physical packet decoder
calls and final copied bytes. `group_pack_ms` measures compiler packing.
`group_restore_ms` measures the fused grouped-window extraction, optional inverse
predictor and destination copy together; these operations cannot be attributed
separate times after fusion. `group_unpack_ms` and `predictor_decode_ms` retain
their separate scopes for the complete-member path used during full verification;
ordinary fused reads do not increment them. `predictor_encode_ms` measures the
compiler's byte transform, and `compression_ms`/`decode_ms` measure the codec phase.
Complete operation timing also includes
metadata, checksums, interpretation, output and identity checks. These disjoint
phase counters are source-handle lifetime values where applicable, not a substitute
for a cold end-to-end clock. Full verification visits every packet once while
restoring each band's original bytes and checking every summary and the complete
logical digest. Its directory scan and subsequent accesses are counted as they
occur, rather than presented as query warm-up.

## Established mechanisms used

COG already arranges TIFF for range access. GDAL's installed creation options are
the comparison authority, including version-specific interleave limits. Zarr v3
indexed sharding similarly combines independently readable chunks and a shard
index. SKV's compact directory, chunk checksums and stored aggregates synthesize
these established mechanisms with Skarve's existing reader and numerical engine.
Performance and promotion depend on complete cold-query evidence.

- [GDAL COG options](https://gdal.org/en/stable/drivers/raster/cog.html)
- [Zarr v3 indexed sharding](https://zarr-specs.readthedocs.io/en/latest/v3/codecs/sharding-indexed/index.html)
- [flate2 pinned codec API](https://docs.rs/flate2/1.1.10/flate2/)

## Decoder implementation and resource contract

Linux serving uses system libdeflate with the per-context allocation API,
qualified at1.19. Other platforms retain flate2/miniz_oxide decoding. This is an
implementation choice, not a new wire codec or required-feature bit. Encoder
bytes, grouped layout, predictors and numerical order are unchanged.

Each native decoder context has a separate, thread-scoped allocator with an
aggregate65,536-byte ceiling including aligned allocation headers. No global
libdeflate allocator is installed, so GDAL's allocator is unaffected. The
context is uniquely owned and freed on its allocating thread on success,
corruption, allocation failure and cancellation. Input/output lengths remain
bounded at4 MiB and grouped packets retain their smaller3,276,800-byte bound.
The context fits the existing8 MiB scratch reservation; it is not added to an
unbounded worker pool. `libdeflate_context_peak_bytes` exposes the maximum
payload-context charge. Native decoding fills one exact-sized output buffer and
requires both exact output length and complete input consumption.

The native call checks the zlib header and Adler32. Invalid headers, dictionaries,
truncated data, incorrect output lengths, trailing bytes and concatenated streams
reject. Cancellation is checked before and after the bounded decode; it cannot
interrupt the C call internally. Checksums and source-generation verification
still precede interpretation, and no original raster is read. Valid stored,
fixed-Huffman and dynamic-Huffman streams are tested against the prior decoder.
Malformed-stream acceptance is not claimed identical: upstream libdeflate permits
some incomplete Huffman codes that other decoders reject. Successful output must
still satisfy all SKV checksum, size, wrapper and logical validation contracts.

The reviewed primary sources are the [v1.19 C API](https://github.com/ebiggers/libdeflate/blob/v1.19/libdeflate.h),
[context allocation and decoding](https://github.com/ebiggers/libdeflate/blob/v1.19/lib/deflate_decompress.c),
and [zlib wrapper checks](https://github.com/ebiggers/libdeflate/blob/v1.19/lib/zlib_decompress.c).
The qualified Linux soname and package are explicit installation dependencies;
there is no silent Linux fallback when that runtime is absent.
