# SKV v0 cold-serving experiment

This is a source-layout experiment, not a new algorithm tournament. Native
strict and exactextract are separate numerical policies. No HM application
pipeline or ordered-sum compatibility speed claim follows from fractional
queries over real demographic pixels.

## Inputs and same-data controls

`fixtures.py` uses the installed system GDAL 3.8.4 and one encoder thread. Public
analytical cubes are 513 x 519 x 36/40 Float32 samples with distinct spatial and
band terms, signed values, zeros, NoData and irregular edge tiles. They are not
one repeated band. Private real inputs are supplied by explicit paths and never
exported with this source: one actual 512 x 512 x 36 native demographic tile,
and the complete available WorldPop BFA single-band raster. Smaller native crops
are development diagnostics and must retain their window and original size.

For each logical dataset retain ordinary stripped DEFLATE/BAND TIFF, COG
128/256 ZSTD level 6 with floating predictor and no overviews, and ordinary
BAND-interleaved tiled ZSTD 256. The latter isolates band-selection effects.
Every raw sample bit, mask, grid, CRS, scale, offset, NoData, band order/name and
unit is independently compared by streamed GDAL reads. No resampling occurs.

Current GDAL COG supports tiled layout, lossless codecs and predictor controls;
its COG interleave option was added in GDAL 3.11, so the installed 3.8.4 COG
writer can produce only pixel interleave. Do not describe SKV's selective-band
advantage over these COGs as a limitation of all newer COG implementations.
[GDAL COG documentation](https://gdal.org/en/stable/drivers/raster/cog.html)
and [GTiff documentation](https://gdal.org/en/stable/drivers/raster/gtiff.html).

The existing research environment also contains Rasterio 1.5.1 with bundled
GDAL 3.12.4. `modern_cog.py` explicitly uses that already available encoder to
add stronger 128-pixel BAND and TILE COG controls. The Skarve decoder remains
system GDAL 3.8.4 for every TIFF/COG lane. Independent full-raster reads through
both encoder and system GDAL verify logical equality. This does not replace the
older controls or claim all lanes use the same encoder version. Preparation is
also measured for these stronger COG controls.

Pre-timing geometric inspection showed that a 256-pixel chunk on a 512-pixel
crop leaves no fully covered chunk inside an inset polygon. Primary SKV
therefore starts at128, retaining256 as an ablation. Existing RSI accepts16,
64 or256 only: retain both64 and256 COG-index controls and their separate
storage/build costs. Select the strongest applicable control using the mixed
development cohort before final geometry is frozen. A1025 x1031 x40 analytical
cube provides a multi-tile interior at both SKV sizes
sizes within the task's resource envelope; the actual512 age tile remains a
separate real-data case rather than being duplicated into a larger synthetic
country.

COG already arranges directories and tile payloads for random access. Zarr v3
indexed sharding already combines independently encoded chunks and an offset/
length index in one shard, permits the index at either end and specifies empty
chunk markers. These are established engineering references, not novelty claims
for SKV. The bounded check motivates independently verifiable pages and small
bootstrap metadata; it does not require another backend implementation.
[Zarr indexed sharding specification](https://zarr-specs.readthedocs.io/en/latest/v3/codecs/sharding-indexed/index.html).

## Primary matrix

Run A one polygon/one band, B multiple polygons/one band, C one polygon/all bands,
D multiple polygons/all bands. One-band selection also uses original band 23
from a many-band object. Main batch size is eight independent overlapping and
disjoint zones; 64-zone and 2/8-band subsets are bounded diagnostics. Include
tiny/subpixel, compact, large interior, boundary-heavy, hole, multipart, edge,
partial-outside and empty-support cases. B/D use one real `cleave` job.

Lanes: native ordinary TIFF, tuned COG, BAND tiled TIFF, COG plus existing
summary-only index, SKV raw-only and summary-enabled; exactextract over ordinary
TIFF/COG/SKV and natural upstream TIFF/COG. Feature and raster strategies are
retained as forced controls. Select any declared default using development
inputs, before final inputs; post-hoc best observed is labelled separately.

The existing persistent index has a20-band registered-source limit. The36/40
band controls use two source mappings and two source-bound indexes within one
shared multi-slice `cleave` job. All required source opens and HTTP index reads
are charged. Outputs are assembled in original band order; the index format
and its limits are preserved. Single-band queries register only the required
group. Exactextract and natural upstream use262144 cells per processing window;
the native bridge explicitly permits256 MiB active window storage, needed for
the40-band cohort. The2 GiB process cap remains enforced for all workers.
Raster HTTP admission allows8192 requests and768 MiB per registered reader,
4 MiB maximum ranges and4 MiB transfer cache. Existing RSI HTTP readers retain
their separate4096-request/128 MiB limits and have no configured transfer cache.
A one-band request uses one reader pair; all36/40 bands require two pairs.
The loopback server also enforces a combined8192-request/1 GiB body ceiling for
the whole job. Do not present a per-reader allowance as the combined allowance.

Native `carve` retains its fixed64 MiB read allocation cap. A few physical
layouts can exceed it even when the source is otherwise readable. The first
development pass preserved this admission failure for the40-band stripped
TIFF and some256-pixel controls. Focused comparisons explicitly declare the
fully verified same-data COG128 BAND source as their numerical reference before
freezing. This changes neither the result definition nor tolerances. Separately
labelled one-zone `cleave` diagnostics use the available256 MiB tile cap; they
are never reported as successful `carve` operations. Indexed queries use128 MiB
for both retained reads and batch tiles, preserving the existing RSI cap.

Keep development corrections as distinct configurations. The report generator
refuses to average overlapping cohorts across separate programmes. An execution
failure, or absence of its declared numerical control, remains visible in the
original frozen receipt even when a later focused programme resolves it.

## Cold boundary and resources

Each independent job is a fresh process. The primary timer starts before source
registration/opening and ends after every result has been consumed and encoded.
Opening, metadata, immutable-generation verification, directory/index reads,
decode, normalization, geometry, aggregation, output and close are recorded.
Process wall time includes interpreter/import/startup and teardown, reported as
a separate inclusive boundary. Nested stage spans are never added twice.

Full fixture and index hashes are checked once per programme setup, with bytes
and time recorded separately; this can warm OS cache. Individual jobs pin
device/inode/length/mtime and use the verified strong server ETag. They do not
rehash full files outside query timing. The engine's own verification remains
inside the query timer. No diagnostic `inspect` call is made after the answer.
Cross-process monotonic boundaries identify which HTTP requests belong to the
source-to-consumed span; complete lifecycle traffic is also retained.

Within-job sharing is valid. No decoded data/index/source handles survive between
jobs. OS and provider caches are uncontrolled: these are client/source-cold,
not cold-disk or measured WAN trials. Balanced randomized order uses immutable
fresh geometry seeds for final evaluation. Report medians/ranges for three
independent lifecycles; claim descriptive p95 only for at least 100 distinct
single-query jobs in a clearly named cohort.

Controlled HTTP regimes fixed before timing: zero added delay/unlimited model
bandwidth; 8 ms per request with 64 MiB/s; 25 ms with 16 MiB/s. The loopback server
actually delays/transfers bytes and records each request timeline, HEAD/GET,
condition presence, requested interval, status and delivered body bytes. It
closes connections equally for every lane and runs one request at a time. These
are explicit models, not measurements of R2 latency. Setup traffic is included.

One timing lane, one compute/encoder thread, at most two build workers across
the task, 2 GiB process address-space cap and 8 GiB task scratch. Source cache,
GDAL cache, decode, contribution and output budgets remain explicit. Compare
complete five common outputs with each backend's provenance. Keep failures and
all measured losses. Storage and conversion/rebuild costs accompany query costs.

## Cloud boundary

Existing historical allowances were scoped read-only to existing objects. They
do not establish permission to upload these derived objects or a new destination.
No R2 actions occur without an exact authorized isolated prefix, object list,
request/body budget and retention. Controlled HTTP success is not R2 success.
