//! Experimental self-contained typed serving objects. No numerical kernel lives
//! here: normalization and stored-state validation come from the source engine.
#[path = "skv_prefetch.rs"]
mod boundary_prefetch;
#[path = "skv_group.rs"]
mod group;
#[cfg(target_os = "linux")]
#[path = "skv_deflate.rs"]
mod native_deflate;
#[path = "skv_native.rs"]
mod native_window;
use crate::{
    io::{RangeSource, RegistrationPrefix, RemoteLimits},
    model::{Grid, Raster, Sum, check_cancel},
    persistent::TileSummary,
    source::{
        BandMetadata, RasterMetadata, RawBandWindow, RawRasterMetadata, RawWindow, ReadMetrics,
        SourceSpec, VerificationPolicy, WindowSource,
    },
    stored_summary::{StoredSummarySource, SummaryLayout, validated_state},
};
use anyhow::{Context, Result, ensure};
#[cfg(any(not(target_os = "linux"), test))]
use flate2::bufread::ZlibDecoder;
use flate2::{Compression, write::ZlibEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const MAGIC: &[u8; 8] = b"SKVRAST\0";
pub const BOOTSTRAP: usize = 16_384;
pub const RECORD: usize = 128;
pub const RECORDS_PER_PAGE: usize = 64;
pub const PAGE: usize = 16 + RECORD * RECORDS_PER_PAGE + 32;
pub const MAX_OBJECT: u64 = 128 * 1024 * 1024 * 1024;
const MAX_METADATA: usize = 65_536;
const PAGE_CACHE: usize = 64;
const DIRECTORY_RANGE_PAGES: usize = 8;
const MAX_READS: usize = 65_536;
const MAX_LEAF_RECORDS: usize = 131_072;
// Previously inadmissible large objects use the existing writer's contiguous
// group-major payload order, checked from bounded neighbor descriptors.
const MAX_LARGE_GROUPED_LEAF_RECORDS: usize = 16_777_216;
pub const VERIFY_WORKING_BYTES: usize = 128 << 20;
const SCHEMA: &str = "native_grid_planar_compensated_v1";
const MAX_PREDICTOR_SCRATCH: usize = 256 * 256 * 8;

fn band_payload_layout() -> String {
    "band".into()
}
fn payload_layout_is_band(value: &str) -> bool {
    value == "band"
}
fn validate_payload_layout(value: &str) -> Result<()> {
    ensure!(
        ["band", "row_group_v1"].contains(&value),
        "unsupported SKV payload layout"
    );
    Ok(())
}

fn no_predictor() -> String {
    "none".into()
}
fn predictor_is_none(value: &str) -> bool {
    value == "none"
}
fn validate_predictor(value: &str) -> Result<()> {
    ensure!(
        ["none", "byte_delta_v1"].contains(&value),
        "unsupported SKV predictor"
    );
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompileOptions {
    pub chunk_edge: usize,
    pub band_group: usize,
    pub payload_layout: String,
    pub codec: String,
    pub predictor: String,
    pub compression_level: u32,
    pub summaries: bool,
    pub working_bytes: usize,
    pub max_output_bytes: u64,
}
impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            chunk_edge: 256,
            band_group: 4,
            payload_layout: band_payload_layout(),
            codec: "deflate".into(),
            predictor: no_predictor(),
            compression_level: 3,
            summaries: true,
            working_bytes: 64 << 20,
            max_output_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}
impl CompileOptions {
    fn validate(&self) -> Result<()> {
        validate_payload_layout(&self.payload_layout)?;
        validate_predictor(&self.predictor)?;
        ensure!(
            [64, 128, 256].contains(&self.chunk_edge),
            "SKV chunk edge must be64,128 or256"
        );
        ensure!(
            (1..=64).contains(&self.band_group),
            "SKV band group must be1..64"
        );
        ensure!(
            ["none", "deflate"].contains(&self.codec.as_str()) && self.compression_level <= 9,
            "unsupported SKV codec or compression level"
        );
        ensure!(
            (16 << 20..=256 << 20).contains(&self.working_bytes),
            "SKV compiler memory must be16..256MiB"
        );
        ensure!(
            self.max_output_bytes >= BOOTSTRAP as u64 && self.max_output_bytes <= MAX_OBJECT,
            "SKV output limit exceeds v0 object budget"
        );
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HierarchyDescription {
    tile_edge: usize,
    levels: Vec<(usize, usize, usize)>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u32,
    grid: Grid,
    raw_metadata: RawRasterMetadata,
    chunk_edge: usize,
    band_group: usize,
    #[serde(
        default = "band_payload_layout",
        skip_serializing_if = "payload_layout_is_band"
    )]
    payload_layout: String,
    codec: String,
    #[serde(default = "no_predictor", skip_serializing_if = "predictor_is_none")]
    predictor: String,
    compression_level: u32,
    summaries: bool,
    hierarchy: HierarchyDescription,
    numerical_schema: String,
    build_id: String,
    original_source_id: String,
    original_identity: Option<Value>,
    logical_digest: String,
    directory_digest: String,
    summary_disabled_reason: Option<String>,
}
impl Header {
    fn feature_flags(&self) -> u32 {
        u32::from(self.summaries)
            | if self.predictor == "byte_delta_v1" {
                2
            } else {
                0
            }
            | if self.raw_metadata.source_overview.is_some() {
                4
            } else {
                0
            }
            | if self.grouped() { 8 } else { 0 }
    }
    fn grouped(&self) -> bool {
        self.payload_layout == "row_group_v1"
    }
    /// Membership uses the worst full-tile shape, so clipped edge tiles have
    /// identical band groups and a smaller packet. Types may differ only when
    /// their original scalar byte widths agree; no values are converted.
    fn band_group_bounds(&self, band: usize) -> Result<(usize, usize)> {
        ensure!(band < self.bands(), "SKV group band out of bounds");
        if !self.grouped() {
            return Ok((band, 1));
        }
        let mut first = 0;
        while first < self.bands() {
            let scalar = self.raw_metadata.bands[first].scalar_type.byte_width();
            let cap = (group::MAX_BYTES / (self.chunk_edge * self.chunk_edge * (scalar + 1)))
                .min(self.band_group);
            ensure!(cap > 0, "SKV scalar cannot fit a grouped packet");
            let count = self.raw_metadata.bands[first..]
                .iter()
                .take(cap)
                .take_while(|b| b.scalar_type.byte_width() == scalar)
                .count();
            if band < first + count {
                return Ok((first, count));
            }
            first += count;
        }
        anyhow::bail!("SKV group membership missing")
    }
    fn canonical_group(&self, id: usize) -> Result<(usize, usize)> {
        let node = id / self.bands();
        let (first, count) = self.band_group_bounds(id % self.bands())?;
        Ok((node * self.bands() + first, count))
    }
    fn groups_per_tile(&self) -> Result<usize> {
        let mut first = 0;
        let mut groups = 0;
        while first < self.bands() {
            first += self.band_group_bounds(first)?.1;
            groups += 1;
        }
        Ok(groups)
    }
    fn layout(&self) -> SummaryLayout {
        SummaryLayout {
            tile_edge: self.hierarchy.tile_edge,
            levels: self.hierarchy.levels.clone(),
        }
    }
    fn bands(&self) -> usize {
        self.raw_metadata.bands.len()
    }
    fn leaves(&self) -> usize {
        self.grid.width.div_ceil(self.chunk_edge) * self.grid.height.div_ceil(self.chunk_edge)
    }
    fn ordered_large_payloads(&self) -> bool {
        self.leaves().saturating_mul(self.bands()) > MAX_LEAF_RECORDS
    }
    /// Canonical physical neighbors in the existing grouped writer schedule.
    /// No allocation proportional to raster extent or band count is required.
    fn physical_neighbors(&self, id: usize) -> Result<(Option<usize>, Option<usize>)> {
        ensure!(self.grouped(), "ordered payload validation requires groups");
        let node = id / self.bands();
        let (first, count) = self.band_group_bounds(id % self.bands())?;
        ensure!(
            node < self.leaves() && id % self.bands() == first,
            "ordered payload descriptor is not a canonical leaf"
        );
        let previous = if node > 0 {
            Some((node - 1) * self.bands() + first)
        } else if first > 0 {
            let (prior, _) = self.band_group_bounds(first - 1)?;
            Some((self.leaves() - 1) * self.bands() + prior)
        } else {
            None
        };
        let next = if node + 1 < self.leaves() {
            Some((node + 1) * self.bands() + first)
        } else if first + count < self.bands() {
            Some(first + count)
        } else {
            None
        };
        Ok((previous, next))
    }
    fn records(&self) -> Result<usize> {
        self.layout()
            .records()?
            .checked_mul(self.bands())
            .context("SKV record count overflow")
    }
    fn pages(&self) -> Result<usize> {
        Ok(self.records()?.div_ceil(RECORDS_PER_PAGE))
    }
    fn data_offset(&self) -> Result<u64> {
        (self.pages()? as u64)
            .checked_mul(PAGE as u64)
            .and_then(|n| n.checked_add(BOOTSTRAP as u64))
            .context("SKV directory size overflow")
    }
    fn validate(&self) -> Result<()> {
        validate_payload_layout(&self.payload_layout)?;
        validate_predictor(&self.predictor)?;
        ensure!(
            self.version == 0 && self.numerical_schema == SCHEMA,
            "unsupported SKV interpretation version"
        );
        self.grid.validate()?;
        ensure!(
            [64, 128, 256].contains(&self.chunk_edge)
                && self.chunk_edge == self.hierarchy.tile_edge,
            "invalid SKV chunk shape"
        );
        ensure!(
            (1..=64).contains(&self.bands()) && (1..=64).contains(&self.band_group),
            "invalid SKV band count/group"
        );
        ensure!(
            ["none", "deflate"].contains(&self.codec.as_str()) && self.compression_level <= 9,
            "unsupported SKV codec"
        );
        ensure!(
            self.raw_metadata.source_band_count >= self.bands()
                && self.raw_metadata.source_band_count <= 65_536
                && self
                    .raw_metadata
                    .source_overview
                    .is_none_or(|level| level < 64)
                && ["Area", "Point", "unspecified"]
                    .contains(&self.raw_metadata.pixel_convention.as_str()),
            "invalid SKV source metadata"
        );
        let mut seen = std::collections::BTreeSet::new();
        for band in &self.raw_metadata.bands {
            ensure!(
                f64::from_bits(band.scale_f64_bits).is_finite()
                    && f64::from_bits(band.offset_f64_bits).is_finite(),
                "invalid SKV scale/offset"
            );
            ensure!(
                band.unit.as_ref().is_none_or(|s| s.len() <= 1024)
                    && band.description.len() <= 4096
                    && band.mask_flags & !15 == 0
                    && band.original_band_index < self.raw_metadata.source_band_count
                    && seen.insert(band.original_band_index),
                "invalid SKV band interpretation"
            );
        }
        self.layout().validate(&self.grid)?;
        ensure!(
            self.hierarchy.levels == SummaryLayout::new(&self.grid, self.chunk_edge, true)?.levels,
            "SKV hierarchy must include every native-grid parent level"
        );
        ensure!(
            self.leaves()
                .checked_mul(self.bands())
                .is_some_and(|n| n <= MAX_LEAF_RECORDS
                    || (self.grouped() && n <= MAX_LARGE_GROUPED_LEAF_RECORDS)),
            "SKV v0 exceeds131072 independent typed leaf chunks or16777216 ordered grouped chunks"
        );
        for value in [&self.build_id, &self.logical_digest, &self.directory_digest] {
            ensure!(
                value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid SKV digest"
            );
        }
        ensure!(
            self.original_source_id.len() <= 1024
                && self
                    .summary_disabled_reason
                    .as_ref()
                    .is_none_or(|s| s.len() <= 256),
            "SKV provenance exceeds budget"
        );
        ensure!(
            self.data_offset()? <= MAX_OBJECT,
            "SKV directory exceeds object budget"
        );
        Ok(())
    }
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(
        bytes[at..at + 4]
            .try_into()
            .expect("fixed checked structure"),
    )
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(
        bytes[at..at + 8]
            .try_into()
            .expect("fixed checked structure"),
    )
}
fn put_u32(bytes: &mut [u8], at: usize, n: u32) {
    bytes[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], at: usize, n: u64) {
    bytes[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
fn encode(input: &[u8], codec: u32, level: u32, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if codec == 0 {
        return Ok(input.to_vec());
    }
    ensure!(codec == 1, "unsupported SKV codec");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    for part in input.chunks(65_536) {
        check_cancel(cancel)?;
        encoder.write_all(part)?;
    }
    let output = encoder.finish()?;
    check_cancel(cancel)?;
    Ok(output)
}
fn decode(input: &[u8], codec: u32, expected: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
    Ok(decode_report(input, codec, expected, cancel)?.0)
}
fn decode_report(
    input: &[u8],
    codec: u32,
    expected: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, usize)> {
    check_cancel(cancel)?;
    ensure!(expected <= 4 << 20, "SKV decoded block exceeds limit");
    if codec == 0 {
        ensure!(input.len() == expected, "SKV uncompressed length mismatch");
        return Ok((input.to_vec(), 0));
    }
    ensure!(codec == 1, "unsupported SKV codec");
    #[cfg(target_os = "linux")]
    {
        let decoded = native_deflate::decode_zlib(input, expected, cancel)?;
        Ok((decoded.bytes, decoded.context_peak_bytes))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok((decode_miniz(input, expected, cancel)?, 0))
    }
}
// Retain the existing decoder for other platforms and a differential test control.
#[cfg(any(not(target_os = "linux"), test))]
fn decode_miniz(input: &[u8], expected: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(input);
    let mut output = Vec::with_capacity(expected);
    // Keep bounded codec scratch off the native caller's stack. Node's FFI
    // workers may have128KiB stacks; codec initialization also needs stack space.
    let mut buffer = vec![0u8; 65_536];
    loop {
        check_cancel(cancel)?;
        let n = decoder
            .read(&mut buffer)
            .context("invalid SKV compressed block")?;
        check_cancel(cancel)?;
        if n == 0 {
            break;
        }
        ensure!(
            output.len().checked_add(n).is_some_and(|n| n <= expected),
            "SKV decompression length overflow"
        );
        output.extend_from_slice(&buffer[..n]);
    }
    ensure!(
        output.len() == expected && decoder.total_in() == input.len() as u64,
        "SKV decompressed length or trailing codec bytes mismatch"
    );
    Ok(output)
}
/// Reversible byte-only transform of the sample prefix. The stored mask suffix
/// is untouched. Planes span the actual tile; each actual row resets its delta.
/// One heap sample buffer is at most 524,288 bytes, already covered by the
/// reader's 8MiB scratch reservation and the compiler's extra 4MiB allowance.
#[allow(clippy::too_many_arguments)]
fn byte_delta(
    payload: &mut [u8],
    sample_bytes: usize,
    width: usize,
    height: usize,
    scalar_bytes: usize,
    inverse: bool,
    cancel: &AtomicBool,
) -> Result<usize> {
    check_cancel(cancel)?;
    ensure!(
        (1..=256).contains(&width)
            && (1..=256).contains(&height)
            && [1, 2, 4, 8].contains(&scalar_bytes),
        "invalid SKV predictor shape"
    );
    let cells = width
        .checked_mul(height)
        .context("SKV predictor size overflow")?;
    ensure!(
        cells.checked_mul(scalar_bytes) == Some(sample_bytes)
            && sample_bytes <= MAX_PREDICTOR_SCRATCH
            && sample_bytes.checked_add(cells) == Some(payload.len()),
        "SKV predictor sample/mask length mismatch"
    );
    let mut scratch = vec![0u8; sample_bytes];
    for plane in 0..scalar_bytes {
        for row in 0..height {
            check_cancel(cancel)?;
            let row_start = row * width;
            let plane_start = plane * cells + row_start;
            let mut previous = 0u8;
            for x in 0..width {
                let sample = (row_start + x) * scalar_bytes + plane;
                let packed = plane_start + x;
                if inverse {
                    previous = previous.wrapping_add(payload[packed]);
                    scratch[sample] = previous;
                } else {
                    let value = payload[sample];
                    scratch[packed] = value.wrapping_sub(previous);
                    previous = value;
                }
            }
        }
    }
    check_cancel(cancel)?;
    payload[..sample_bytes].copy_from_slice(&scratch);
    Ok(sample_bytes)
}
fn encode_header(header: &Header, length: u64, cancel: &AtomicBool) -> Result<Vec<u8>> {
    header.validate()?;
    let metadata = serde_json::to_vec(header)?;
    ensure!(
        metadata.len() <= MAX_METADATA,
        "SKV metadata exceeds decoded limit"
    );
    let encoded = encode(&metadata, 1, 3, cancel)?;
    ensure!(
        encoded.len() <= BOOTSTRAP - 96,
        "SKV metadata exceeds bounded bootstrap"
    );
    let mut bytes = vec![0; BOOTSTRAP];
    bytes[..8].copy_from_slice(MAGIC);
    put_u32(&mut bytes, 12, header.feature_flags());
    put_u32(&mut bytes, 16, BOOTSTRAP as u32);
    put_u32(&mut bytes, 20, 1);
    put_u32(&mut bytes, 24, encoded.len() as u32);
    put_u32(&mut bytes, 28, metadata.len() as u32);
    put_u64(&mut bytes, 32, length);
    put_u64(&mut bytes, 40, BOOTSTRAP as u64);
    put_u64(&mut bytes, 48, header.pages()? as u64);
    put_u64(&mut bytes, 56, header.data_offset()?);
    bytes[64..64 + encoded.len()].copy_from_slice(&encoded);
    let digest = blake3::hash(&bytes[..BOOTSTRAP - 32]);
    bytes[BOOTSTRAP - 32..].copy_from_slice(digest.as_bytes());
    Ok(bytes)
}
fn decode_header(bytes: &[u8], length: u64, cancel: &AtomicBool) -> Result<Header> {
    ensure!(
        bytes.len() == BOOTSTRAP && &bytes[..8] == MAGIC,
        "not a complete SKV v0 bootstrap"
    );
    ensure!(
        blake3::hash(&bytes[..BOOTSTRAP - 32]).as_bytes() == &bytes[BOOTSTRAP - 32..],
        "SKV bootstrap checksum mismatch"
    );
    ensure!(
        u32_at(bytes, 8) == 0
            && u32_at(bytes, 12) & !15 == 0
            && u32_at(bytes, 16) == BOOTSTRAP as u32
            && u32_at(bytes, 20) == 1
            && u64_at(bytes, 32) == length
            && length <= MAX_OBJECT
            && u64_at(bytes, 40) == BOOTSTRAP as u64,
        "unsupported SKV version/features/length"
    );
    let (encoded, decoded) = (u32_at(bytes, 24) as usize, u32_at(bytes, 28) as usize);
    ensure!(
        encoded > 0 && encoded <= BOOTSTRAP - 96 && decoded > 0 && decoded <= MAX_METADATA,
        "SKV metadata allocation bound exceeded"
    );
    ensure!(
        bytes[64 + encoded..BOOTSTRAP - 32].iter().all(|&b| b == 0),
        "noncanonical SKV bootstrap padding"
    );
    let header: Header =
        serde_json::from_slice(&decode(&bytes[64..64 + encoded], 1, decoded, cancel)?)?;
    header.validate()?;
    ensure!(
        header.pages()? as u64 == u64_at(bytes, 48)
            && header.data_offset()? == u64_at(bytes, 56)
            && header.data_offset()? <= length
            && header.feature_flags() == u32_at(bytes, 12),
        "SKV directory/header disagreement"
    );
    Ok(header)
}

enum Store {
    Local {
        file: File,
        path: PathBuf,
        signature: String,
        length: u64,
    },
    Remote(RangeSource),
}
// SKV serving is self-contained. GeoTIFF mask/PAM siblings do not participate
// in this generation pin, and neither source paths nor credentials are exposed.
fn local_signature_for(path: &Path, stat: &fs::Metadata) -> Result<String> {
    ensure!(stat.is_file(), "SKV source must be a regular file");
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update(stat.len().to_le_bytes());
    digest.update(format!("{:?}", stat.modified()?).as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(stat.dev().to_le_bytes());
        digest.update(stat.ino().to_le_bytes());
        digest.update(stat.ctime().to_le_bytes());
        digest.update(stat.ctime_nsec().to_le_bytes());
    }
    Ok(format!("skv-local-stat-sha256:{:x}", digest.finalize()))
}
fn local_signature(path: &Path) -> Result<String> {
    local_signature_for(path, &fs::metadata(path)?)
}
impl Store {
    fn local(path: &str) -> Result<Self> {
        let path = fs::canonicalize(path).context("SKV source not found")?;
        let signature = local_signature(&path)?;
        let file = File::open(&path)?;
        let length = file.metadata()?.len();
        ensure!(
            length >= BOOTSTRAP as u64 && length <= MAX_OBJECT,
            "SKV object length exceeds limit"
        );
        let store = Self::Local {
            file,
            path,
            signature,
            length,
        };
        store.verify_local()?;
        Ok(store)
    }
    fn length(&self) -> u64 {
        match self {
            Self::Local { length, .. } => *length,
            Self::Remote(s) => s.length,
        }
    }
    fn verify_local(&self) -> Result<()> {
        if let Self::Local {
            path,
            signature,
            file,
            length,
        } = self
        {
            let opened = file.metadata()?;
            ensure!(
                local_signature(path)? == *signature
                    && local_signature_for(path, &opened)? == *signature
                    && opened.len() == *length,
                "SKV serving object changed"
            );
        }
        Ok(())
    }
    fn verify(&mut self) -> Result<()> {
        match self {
            Self::Local { .. } => self.verify_local(),
            Self::Remote(s) => s.verify_remote(),
        }
    }
    fn read(&mut self, offset: u64, length: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
        check_cancel(cancel)?;
        ensure!(
            length > 0
                && length <= 4 << 20
                && offset
                    .checked_add(length as u64)
                    .is_some_and(|n| n <= self.length()),
            "SKV range out of bounds"
        );
        self.verify_local()?;
        let bytes = match self {
            Self::Local { file, .. } => {
                file.seek(SeekFrom::Start(offset))?;
                let mut v = vec![0; length];
                for chunk in v.chunks_mut(65_536) {
                    check_cancel(cancel)?;
                    file.read_exact(chunk)?;
                }
                v
            }
            Self::Remote(source) => source.read_range_cancellable(offset, length as u64, cancel)?,
        };
        check_cancel(cancel)?;
        self.verify_local()?;
        Ok(bytes)
    }
    fn diagnostics(&self) -> Value {
        match self {
            Self::Local { .. } => json!({"kind":"local","authority":"pinned_file_stat"}),
            Self::Remote(s) => {
                json!({"kind":"http","authority":"strong_etag_conditional_ranges","metrics":s.metrics})
            }
        }
    }
    fn generation(&self) -> &str {
        match self {
            Self::Local { signature, .. } => signature,
            Self::Remote(source) => &source.source_id,
        }
    }
    fn remote_metrics(&self) -> Option<&crate::io::RemoteMetrics> {
        match self {
            Self::Remote(source) => Some(&source.metrics),
            _ => None,
        }
    }
    fn raw_range_limit(&self) -> usize {
        match self {
            Self::Local { .. } => 4 << 20,
            Self::Remote(source) => source.range_byte_limit().min(4 << 20) as usize,
        }
    }
    fn directory_range_pages(&self) -> usize {
        match self {
            Self::Local { .. } => 1,
            Self::Remote(source) => {
                (source.range_byte_limit() as usize / PAGE).min(DIRECTORY_RANGE_PAGES)
            }
        }
    }
}
#[derive(Serialize)]
struct ReadRecord {
    kind: &'static str,
    offset: u64,
    length: usize,
    start_ms: f64,
    duration_ms: f64,
}
#[derive(Default, Serialize)]
struct Metrics {
    logical_reads: usize,
    logical_bytes: u64,
    bootstrap_bytes: u64,
    directory_bytes: u64,
    raw_encoded_bytes: u64,
    raw_decoded_bytes: u64,
    decoder_calls: usize,
    coalesced_raw_ranges: usize,
    coalesced_raw_chunks: usize,
    materialized_cells: u64,
    copied_bytes: u64,
    interval_insert_shift_bytes: u64,
    page_cache_hits: usize,
    directory_pages_prefetched: usize,
    page_cache_evictions: usize,
    page_cache_peak_bytes: usize,
    summary_states_read: usize,
    decode_ms: f64,
    libdeflate_context_peak_bytes: usize,
    group_unpack_ms: f64,
    /// Fused sample/mask extraction, optional inverse predictor and final copy.
    /// Separate from the unfused full-verification component timers.
    group_restore_ms: f64,
    predictor_decode_ms: f64,
    predictor_scratch_peak_bytes: usize,
    metadata_decode_ms: f64,
    normalization_ms: f64,
    /// Normalized source-window instrumentation; typed raw reads do not increment these.
    native_window_calls: u64,
    native_normalized_written_bytes: u64,
    native_row_scratch_peak_bytes: usize,
    native_window_cells: u64,
    native_raw_intermediate_allocated_bytes: u64,
    native_output_allocated_bytes: u64,
    native_window_validation_ms: f64,
    boundary_prefetch_calls: usize,
    boundary_prefetch_admissions: usize,
    boundary_prefetch_ranges: usize,
    boundary_prefetch_encoded_bytes: u64,
    boundary_prefetch_planned_request_savings: usize,
    boundary_prefetch_plan_ms: f64,
    boundary_prefetch_fetch_ms: f64,
    boundary_prefetch_scratch_bound_bytes: usize,
    boundary_prefetch_demand_cache_charge_peak_bytes: usize,
    read_ms: f64,
    verified_whole_file_bytes: u64,
    records: Vec<ReadRecord>,
}
struct ReaderState {
    store: Store,
    metrics: Metrics,
    pages: VecDeque<(usize, Vec<u8>)>,
    /// Sorted by begin; fixed capacity makes the retained allocation explicit.
    /// Most sequential reads append. Unordered reads use a bounded binary-search
    /// insertion; the payload layout itself is unchanged.
    intervals: Vec<(u64, u64, usize)>,
    started: Instant,
    logical_read_limit: usize,
    exhaustive_ordered_verify: bool,
}
impl ReaderState {
    fn read(
        &mut self,
        kind: &'static str,
        offset: u64,
        length: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.metrics.logical_reads < self.logical_read_limit,
            "SKV logical read budget exceeded"
        );
        let start = Instant::now();
        let begin_ms = self.started.elapsed().as_secs_f64() * 1000.;
        let value = self.store.read(offset, length, cancel)?;
        ensure!(
            self.store
                .remote_metrics()
                .is_none_or(|m| m.ranges.capacity() <= MAX_READS),
            "SKV remote request log capacity exceeds admission"
        );
        let elapsed = start.elapsed().as_secs_f64() * 1000.;
        self.metrics.logical_reads += 1;
        self.metrics.logical_bytes += length as u64;
        self.metrics.read_ms += elapsed;
        match kind {
            "bootstrap" => self.metrics.bootstrap_bytes += length as u64,
            "directory" => self.metrics.directory_bytes += length as u64,
            "raw" => self.metrics.raw_encoded_bytes += length as u64,
            _ => {}
        }
        if self.metrics.records.len() < 4096 {
            self.metrics.records.push(ReadRecord {
                kind,
                offset,
                length,
                start_ms: begin_ms,
                duration_ms: elapsed,
            });
        }
        Ok(value)
    }
}
// Bounded lookahead descriptors. Physical ordering is independent of
// native output band order and mathematical contribution order.
struct Pending {
    out_index: usize,
    band: usize,
    id: usize,
    bounds: [usize; 4],
    record: [u8; RECORD],
}
fn selected_group_members(grouped: bool, entry: &Pending, remaining: &[Pending]) -> usize {
    if grouped {
        let leader = u64_at(&entry.record, 112);
        1 + remaining
            .iter()
            .take_while(|e| u64_at(&e.record, 112) == leader)
            .count()
    } else {
        1
    }
}
fn pending_range_fits(
    offset: u64,
    length: usize,
    count: usize,
    next: &Pending,
    members: usize,
    limit: usize,
) -> bool {
    u64_at(&next.record, 0) == offset + length as u64
        && length.saturating_add(u32_at(&next.record, 8) as usize) <= limit
        && count + members <= 256
}
pub struct SkvSource {
    header: Header,
    metadata: RasterMetadata,
    raw_metadata: RawRasterMetadata,
    mapping: Vec<usize>,
    layout: SummaryLayout,
    state: RefCell<ReaderState>,
    invalidated: Cell<bool>,
    summaries: bool,
    boundary_read_ahead: bool,
    root_digest: String,
    retained: usize,
    descriptor: Value,
}
impl SkvSource {
    pub fn open(spec: &SourceSpec, cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        crate::source::validate_source_spec(spec)?;
        if spec.location.starts_with("http://") || spec.location.starts_with("https://") {
            let (source, prefix) = RangeSource::register_configured_prefix(
                &spec.location,
                RemoteLimits {
                    max_requests: spec.http.max_requests,
                    max_download_bytes: spec.http.max_download_bytes,
                    max_range_bytes: spec.http.max_range_bytes,
                    timeout_seconds: spec.http.timeout_seconds,
                },
                Arc::new(AtomicBool::new(false)),
                Some(&spec.http),
                BOOTSTRAP,
                spec.identity
                    .as_ref()
                    .and_then(|identity| identity.etag.as_deref()),
                cancel,
            )?;
            Self::from_store_with_prefix(spec, Store::Remote(source), cancel, Some(prefix))
        } else {
            Self::from_store(spec, Store::local(&spec.location)?, cancel)
        }
    }
    pub(crate) fn open_registered(
        spec: &SourceSpec,
        cancel: &AtomicBool,
        source: RangeSource,
    ) -> Result<Self> {
        Self::from_store(spec, Store::Remote(source), cancel)
    }
    fn from_store(spec: &SourceSpec, store: Store, cancel: &AtomicBool) -> Result<Self> {
        Self::from_store_with_prefix(spec, store, cancel, None)
    }
    fn from_store_with_prefix(
        spec: &SourceSpec,
        store: Store,
        cancel: &AtomicBool,
        prefix: Option<RegistrationPrefix>,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        crate::source::validate_source_spec(spec)?;
        ensure!(
            spec.variable.is_none() && spec.overview.is_none() && spec.longitude_shift == 0,
            "SKV does not support source reinterpretation selectors"
        );
        ensure!(
            spec.http.cache_bytes <= 8 << 20
                && spec.http.max_range_bytes <= 4 << 20
                && spec.http.max_requests <= 65_536
                && spec.http.max_download_bytes <= 768 << 20
                && spec.http.timeout_seconds <= 30,
            "SKV transport options exceed source limits"
        );
        let length = store.length();
        let mut state = ReaderState {
            store,
            metrics: Metrics {
                records: Vec::with_capacity(4096),
                ..Metrics::default()
            },
            pages: VecDeque::with_capacity(PAGE_CACHE),
            intervals: Vec::new(),
            logical_read_limit: MAX_READS,
            exhaustive_ordered_verify: false,
            started: prefix.as_ref().map_or_else(Instant::now, |p| p.started),
        };
        let bytes = if let Some(prefix) = prefix {
            ensure!(
                prefix.bytes.len() == BOOTSTRAP,
                "SKV bootstrap length mismatch"
            );
            // The initial GET was charged by RangeSource. Consume its owned bytes
            // once even with cache0, and record the same logical bootstrap read.
            state.metrics.logical_reads = 1;
            state.metrics.logical_bytes = BOOTSTRAP as u64;
            state.metrics.bootstrap_bytes = BOOTSTRAP as u64;
            state.metrics.read_ms = prefix.duration_ms;
            state.metrics.records.push(ReadRecord {
                kind: "bootstrap",
                offset: 0,
                length: BOOTSTRAP,
                start_ms: 0.,
                duration_ms: prefix.duration_ms,
            });
            prefix.bytes
        } else {
            state.read("bootstrap", 0, BOOTSTRAP, cancel)?
        };
        let metadata_started = Instant::now();
        let header = decode_header(&bytes, length, cancel)?;
        state.metrics.metadata_decode_ms = metadata_started.elapsed().as_secs_f64() * 1000.;
        let root_digest = blake3::hash(&bytes).to_hex().to_string();
        if let Some(crs) = &spec.crs {
            ensure!(
                crs == &header.grid.crs,
                "SKV CRS override conflicts with stored grid"
            );
        }
        if let Some(identity) = &spec.identity {
            ensure!(
                identity.byte_length == length
                    && identity.sha256.len() == 64
                    && identity.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "SKV content identity mismatch"
            );
            if let Store::Remote(source) = &state.store {
                if let Some(etag) = &identity.etag {
                    ensure!(etag == &source.etag, "SKV expected ETag mismatch");
                }
                if identity.policy == VerificationPolicy::TrustedManifest {
                    ensure!(
                        identity.etag.is_some(),
                        "remote SKV trusted manifest requires ETag"
                    );
                }
            }
            if identity.policy == VerificationPolicy::Verify {
                let mut hash = Sha256::new();
                let mut offset = 0;
                while offset < length {
                    let n = (length - offset).min(spec.http.max_range_bytes.min(4 << 20)) as usize;
                    ensure!(n > 0, "invalid SKV verification range limit");
                    hash.update(state.read("verification", offset, n, cancel)?);
                    offset += n as u64;
                }
                ensure!(
                    format!("{:x}", hash.finalize()) == identity.sha256.to_ascii_lowercase(),
                    "SKV content SHA256 mismatch"
                );
                state.metrics.verified_whole_file_bytes = length;
            }
        }
        let mapping = spec
            .bands
            .clone()
            .unwrap_or_else(|| (0..header.bands()).collect());
        ensure!(
            !mapping.is_empty()
                && mapping.len() <= 64
                && mapping
                    .iter()
                    .enumerate()
                    .all(|(i, b)| *b < header.bands() && !mapping[..i].contains(b)),
            "invalid SKV band mapping"
        );
        let raw_metadata = RawRasterMetadata {
            bands: mapping
                .iter()
                .map(|&i| header.raw_metadata.bands[i].clone())
                .collect(),
            pixel_convention: header.raw_metadata.pixel_convention.clone(),
            source_band_count: header.raw_metadata.source_band_count,
            source_overview: header.raw_metadata.source_overview,
        };
        let (serving_identity, identity_authority) = if let Some(identity) = &spec.identity {
            (
                json!({"content_sha256":identity.sha256.to_ascii_lowercase(),"byte_length":length}),
                if identity.policy == VerificationPolicy::Verify {
                    "verified_content_sha256"
                } else {
                    "explicit_trusted_manifest_sha256"
                },
            )
        } else {
            (
                json!({"generation":state.store.generation(),"byte_length":length}),
                "serving_object_generation",
            )
        };
        let source_key=blake3::hash(&serde_json::to_vec(&json!({"bootstrap_blake3":root_digest,"band_mapping":mapping,"serving_identity":serving_identity}))?).to_hex().to_string();
        let metadata = RasterMetadata {
            grid: header.grid.clone(),
            source_id: format!("skv-v0-source:{source_key}"),
            bands: raw_metadata
                .bands
                .iter()
                .map(|b| BandMetadata {
                    data_type: format!("{:?}", b.scalar_type),
                    nodata: b.nodata_f64_bits.map(f64::from_bits),
                    scale: f64::from_bits(b.scale_f64_bits),
                    offset: f64::from_bits(b.offset_f64_bits),
                    unit: b.unit.clone(),
                    block_size: (header.chunk_edge, header.chunk_edge),
                })
                .collect(),
        };
        let descriptor = json!({"format":"skv_v0","root_blake3":root_digest,"build_id":header.build_id,
            "logical_digest":header.logical_digest,"serving_length":length,"band_mapping":mapping,
            "interpretation":"skarve_normalized_f64_v1","serving_identity":serving_identity,"identity_authority":identity_authority,
            "bootstrap_digest_scope":"bootstrap_only_not_a_merkle_root","cold_integrity":"serving_generation_and_independent_page_payload_checksums",
            "whole_content_sha256_verified":state.metrics.verified_whole_file_bytes==length});
        let layout = header.layout();
        let summaries = header.summaries && spec.use_summaries;
        // Finite Rust-owned capacities: transfer payloads are separately charged
        // by RangeSource; header/decoded metadata and client state have a2MiB
        // reserve. Per-read buffers are additional in read_buffer_bound.
        let interval_capacity = if header.ordered_large_payloads() {
            MAX_LEAF_RECORDS
        } else {
            header.leaves() * header.groups_per_tile()?
        };
        let retained = spec
            .http
            .cache_bytes
            .saturating_add(PAGE_CACHE * (PAGE + std::mem::size_of::<(usize, Vec<u8>)>()))
            .saturating_add(interval_capacity * std::mem::size_of::<(u64, u64, usize)>())
            .saturating_add(4096 * std::mem::size_of::<ReadRecord>())
            .saturating_add(MAX_READS * std::mem::size_of::<crate::io::RemoteRange>())
            .saturating_add(2 << 20);
        ensure!(
            retained <= crate::source::NATIVE_SOURCE_RETAINED_BYTES,
            "SKV retained source capacity exceeds admission"
        );
        state.intervals = Vec::with_capacity(interval_capacity);
        Ok(Self {
            header,
            metadata,
            raw_metadata,
            mapping,
            layout,
            state: RefCell::new(state),
            invalidated: Cell::new(false),
            summaries,
            boundary_read_ahead: spec.http.boundary_read_ahead,
            root_digest,
            retained,
            descriptor,
        })
    }
    fn guarded<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        ensure!(
            !self.invalidated.get(),
            "SKV reader invalidated; close and reopen"
        );
        let result = operation();
        if result.is_err() {
            self.invalidated.set(true);
        }
        result
    }
    fn bounds(&self, node: usize) -> Result<[usize; 4]> {
        self.layout.bounds(&self.header.grid, node)
    }
    fn descriptor(&self, id: usize, cancel: &AtomicBool) -> Result<[u8; RECORD]> {
        check_cancel(cancel)?;
        ensure!(
            id < self.header.records()?,
            "SKV record index out of bounds"
        );
        let page_id = id / RECORDS_PER_PAGE;
        let mut state = self.state.borrow_mut();
        let page = if let Some(position) = state.pages.iter().position(|(n, _)| *n == page_id) {
            state.metrics.page_cache_hits += 1;
            state.pages.remove(position).expect("checked page position")
        } else {
            let cap = state.store.directory_range_pages();
            ensure!(cap > 0, "SKV directory page exceeds range limit");
            let first = page_id / cap * cap;
            let count = cap.min(self.header.pages()? - first);
            let bytes = state.read(
                "directory",
                BOOTSTRAP as u64 + first as u64 * PAGE as u64,
                count * PAGE,
                cancel,
            )?;
            // Validate every fetched neighbor before publishing any page. No
            // raw bytes are fetched, and record-level checks still happen only
            // for the requested record below. Sparse access can overread pages.
            for (i, page) in bytes.chunks_exact(PAGE).enumerate() {
                check_cancel(cancel)?;
                validate_page(&self.header, first + i, page)?;
            }
            check_cancel(cancel)?;
            state.metrics.directory_pages_prefetched += count - 1;
            for (i, page) in bytes.chunks_exact(PAGE).enumerate() {
                let fetched = first + i;
                if let Some(position) = state.pages.iter().position(|(n, _)| *n == fetched) {
                    state.pages.remove(position);
                } else if state.pages.len() == PAGE_CACHE {
                    state.pages.pop_front();
                    state.metrics.page_cache_evictions += 1;
                }
                state.pages.push_back((fetched, page.to_vec()));
            }
            state.metrics.page_cache_peak_bytes = state
                .metrics
                .page_cache_peak_bytes
                .max(state.pages.len() * PAGE);
            let position = state
                .pages
                .iter()
                .position(|(n, _)| *n == page_id)
                .expect("requested page belongs to validated directory range");
            state.pages.remove(position).expect("checked page position")
        };
        let at = 16 + (id % RECORDS_PER_PAGE) * RECORD;
        let bytes: [u8; RECORD] = page.1[at..at + RECORD].try_into()?;
        if state.pages.len() == PAGE_CACHE {
            state.pages.pop_front();
            state.metrics.page_cache_evictions += 1;
        }
        state.pages.push_back(page);
        state.metrics.page_cache_peak_bytes = state
            .metrics
            .page_cache_peak_bytes
            .max(state.pages.len() * PAGE);
        validate_record(&self.header, id, &bytes, state.store.length())?;
        Ok(bytes)
    }
    fn record(&self, id: usize, cancel: &AtomicBool) -> Result<[u8; RECORD]> {
        let bytes = self.descriptor(id, cancel)?;
        let mut state = self.state.borrow_mut();
        if id / self.header.bands() < self.header.leaves() {
            let (leader, _) = self.header.canonical_group(id)?;
            if self.header.grouped() && id != leader {
                // A nonleader cannot invent another range with the same group
                // identity. Authenticate its descriptor against the canonical
                // record; only that leader enters the physical interval set.
                drop(state);
                let canonical = self.record(leader, cancel)?;
                ensure!(
                    bytes[..28] == canonical[..28]
                        && bytes[32..64] == canonical[32..64]
                        && bytes[112..] == canonical[112..],
                    "SKV group alias differs from canonical leader"
                );
                return Ok(bytes);
            }
            let (begin, end) = (
                u64_at(&bytes, 0),
                u64_at(&bytes, 0) + u32_at(&bytes, 8) as u64,
            );
            if self.header.ordered_large_payloads() {
                let length = state.store.length();
                drop(state);
                let (previous, next) = self.header.physical_neighbors(id)?;
                let expected_begin = if let Some(prior) = previous {
                    let record = self.descriptor(prior, cancel)?;
                    u64_at(&record, 0)
                        .checked_add(u32_at(&record, 8) as u64)
                        .context("SKV prior payload end overflow")?
                } else {
                    self.header.data_offset()?
                };
                let expected_end = if let Some(next) = next {
                    u64_at(&self.descriptor(next, cancel)?, 0)
                } else {
                    length
                };
                ensure!(
                    begin == expected_begin && end == expected_end,
                    "SKV ordered payload gap or overlap"
                );
                state = self.state.borrow_mut();
                // A complete audit checks every adjacency, which proves global
                // contiguity. Partial serving also retains the historical
                // cross-visited overlap check in a fixed-capacity set; exhaustion
                // fails closed rather than dropping evidence or growing memory.
                if state.exhaustive_ordered_verify {
                    return Ok(bytes);
                }
            }
            match state
                .intervals
                .binary_search_by_key(&begin, |entry| entry.0)
            {
                Ok(position) => ensure!(
                    state.intervals[position] == (begin, end, id),
                    "overlapping SKV payload records"
                ),
                Err(position) => {
                    ensure!(
                        position == 0 || state.intervals[position - 1].1 <= begin,
                        "overlapping SKV payload records"
                    );
                    ensure!(
                        position == state.intervals.len() || end <= state.intervals[position].0,
                        "overlapping SKV payload records"
                    );
                    ensure!(
                        state.intervals.len() < state.intervals.capacity(),
                        "SKV visited interval budget exceeded"
                    );
                    state.metrics.interval_insert_shift_bytes += ((state.intervals.len()
                        - position)
                        * std::mem::size_of::<(u64, u64, usize)>())
                        as u64;
                    state.intervals.insert(position, (begin, end, id));
                }
            }
        }
        Ok(bytes)
    }
    fn payload(&self, id: usize, record: &[u8; RECORD], cancel: &AtomicBool) -> Result<Vec<u8>> {
        let encoded = self.state.borrow_mut().read(
            "raw",
            u64_at(record, 0),
            u32_at(record, 8) as usize,
            cancel,
        )?;
        self.decode_payload(id, record, &encoded, cancel)
    }
    fn decode_payload(
        &self,
        id: usize,
        record: &[u8; RECORD],
        encoded: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let mut decoded = self.decode_packet(id, record, encoded, cancel)?;
        if self.header.grouped() {
            return self.group_member(id, record, &decoded, cancel);
        }
        self.inverse_band(id, record, &mut decoded, cancel)?;
        Ok(decoded)
    }
    fn decode_packet(
        &self,
        id: usize,
        record: &[u8; RECORD],
        encoded: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        check_cancel(cancel)?;
        let hash = payload_hash(&self.header, id, record, encoded)?;
        ensure!(
            hash.as_bytes() == &record[32..64],
            "SKV payload checksum mismatch"
        );
        let start = Instant::now();
        let (decoded, context_peak) = decode_report(
            encoded,
            u32_at(record, 24),
            u32_at(record, 12) as usize,
            cancel,
        )?;
        let decode_ms = start.elapsed().as_secs_f64() * 1000.;
        let count = self.header.canonical_group(id)?.1;
        let mut state = self.state.borrow_mut();
        state.metrics.decode_ms += decode_ms;
        state.metrics.libdeflate_context_peak_bytes = state
            .metrics
            .libdeflate_context_peak_bytes
            .max(context_peak);
        state.metrics.decoder_calls += usize::from(u32_at(record, 24) != 0);
        state.metrics.raw_decoded_bytes += decoded.len() as u64;
        state.metrics.materialized_cells +=
            (decoded.len() - u64_at(record, 16) as usize * count) as u64;
        Ok(decoded)
    }
    fn group_member(
        &self,
        id: usize,
        record: &[u8; RECORD],
        packed: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let node = id / self.header.bands();
        let band = id % self.header.bands();
        let (leader, count) = self.header.canonical_group(id)?;
        let [x0, y0, x1, y1] = self.bounds(node)?;
        let start = Instant::now();
        let mut decoded = group::extract(
            packed,
            id - leader,
            count,
            x1 - x0,
            y1 - y0,
            self.header.raw_metadata.bands[band]
                .scalar_type
                .byte_width(),
            self.header.predictor == "byte_delta_v1",
            cancel,
        )?;
        self.state.borrow_mut().metrics.group_unpack_ms += start.elapsed().as_secs_f64() * 1000.;
        self.inverse_band(id, record, &mut decoded, cancel)?;
        Ok(decoded)
    }
    fn inverse_band(
        &self,
        id: usize,
        record: &[u8; RECORD],
        decoded: &mut [u8],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut predictor_ms = 0.;
        let mut predictor_scratch = 0;
        if self.header.predictor == "byte_delta_v1" {
            let start = Instant::now();
            let node = id / self.header.bands();
            let band = id % self.header.bands();
            let [x0, y0, x1, y1] = self.layout.bounds(&self.header.grid, node)?;
            predictor_scratch = byte_delta(
                decoded,
                u64_at(record, 16) as usize,
                x1 - x0,
                y1 - y0,
                self.header.raw_metadata.bands[band]
                    .scalar_type
                    .byte_width(),
                true,
                cancel,
            )?;
            predictor_ms = start.elapsed().as_secs_f64() * 1000.;
        }
        let mut state = self.state.borrow_mut();
        state.metrics.predictor_decode_ms += predictor_ms;
        state.metrics.predictor_scratch_peak_bytes = state
            .metrics
            .predictor_scratch_peak_bytes
            .max(predictor_scratch);
        Ok(())
    }
    fn validate_window(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
    ) -> Result<usize> {
        ensure!(
            w > 0
                && h > 0
                && x.checked_add(w)
                    .is_some_and(|n| n <= self.metadata.grid.width)
                && y.checked_add(h)
                    .is_some_and(|n| n <= self.metadata.grid.height),
            "SKV window out of bounds"
        );
        ensure!(
            !bands.is_empty()
                && bands.len() <= self.max_read_bands()
                && bands
                    .iter()
                    .enumerate()
                    .all(|(i, b)| *b < self.mapping.len() && !bands[..i].contains(b)),
            "SKV reads require1..64 distinct mapped bands"
        );
        w.checked_mul(h).context("SKV window size overflow")
    }
    pub fn inspect_format(&self) -> Value {
        json!({"format":"skv","version":0,"unstable":true,"header":self.header,
            "root_blake3":self.root_digest,"summaries_available":self.summaries,
            "self_contained":true,"original_source_opened":false,"diagnostics":self.diagnostics()})
    }
}
fn validate_page(header: &Header, id: usize, page: &[u8]) -> Result<()> {
    ensure!(
        page.len() == PAGE
            && &page[..4] == b"SKVP"
            && page[4..6] == [0, 0]
            && u64_at(page, 8) == id as u64,
        "invalid SKV directory page"
    );
    ensure!(
        blake3::hash(&page[..PAGE - 32]).as_bytes() == &page[PAGE - 32..],
        "SKV directory checksum mismatch"
    );
    let count = (header.records()? - id * RECORDS_PER_PAGE).min(RECORDS_PER_PAGE);
    ensure!(
        u16::from_le_bytes(page[6..8].try_into()?) as usize == count
            && page[16 + count * RECORD..PAGE - 32].iter().all(|&b| b == 0),
        "SKV directory occupancy mismatch"
    );
    Ok(())
}
fn validate_record(header: &Header, id: usize, bytes: &[u8; RECORD], length: u64) -> Result<()> {
    ensure!(
        u64_at(bytes, 104) == id as u64
            && bytes[124..].iter().all(|&b| b == 0)
            && u32_at(bytes, 28) & !3 == 0,
        "SKV record id/features mismatch"
    );
    let node = id / header.bands();
    let leaf = node < header.leaves();
    ensure!(
        (u32_at(bytes, 28) & 1 != 0) == leaf,
        "SKV record kind mismatch"
    );
    let [x0, y0, x1, y1] = header.layout().bounds(&header.grid, node)?;
    let cells = (x1 - x0)
        .checked_mul(y1 - y0)
        .context("SKV record cell overflow")?;
    if leaf {
        let sample = cells
            .checked_mul(
                header.raw_metadata.bands[id % header.bands()]
                    .scalar_type
                    .byte_width(),
            )
            .context("SKV sample size overflow")?;
        let band_decoded = sample
            .checked_add(cells)
            .context("SKV chunk size overflow")?;
        let (leader, count) = header.canonical_group(id)?;
        let decoded = band_decoded
            .checked_mul(count)
            .context("SKV group length overflow")?;
        if header.grouped() {
            ensure!(
                u64_at(bytes, 112) == leader as u64
                    && u32_at(bytes, 120) == count as u32
                    && decoded <= group::MAX_BYTES,
                "SKV canonical group membership mismatch"
            );
        } else {
            ensure!(
                bytes[112..].iter().all(|&b| b == 0),
                "SKV band record has group fields"
            );
        }
        let begin = u64_at(bytes, 0);
        let encoded = u32_at(bytes, 8) as usize;
        ensure!(
            sample as u64 == u64_at(bytes, 16)
                && decoded == u32_at(bytes, 12) as usize
                && encoded > 0
                && encoded <= 4 << 20
                && (!header.grouped() || encoded <= decoded)
                && begin >= header.data_offset()?
                && begin
                    .checked_add(encoded as u64)
                    .is_some_and(|end| end <= length)
                && u32_at(bytes, 24) <= 1,
            "invalid SKV payload shape/codec/range"
        );
        if u32_at(bytes, 24) == 0 {
            ensure!(encoded == decoded, "SKV raw length mismatch");
        }
    } else {
        ensure!(
            bytes[..28].iter().all(|&b| b == 0)
                && bytes[32..64].iter().all(|&b| b == 0)
                && bytes[112..].iter().all(|&b| b == 0),
            "SKV parent contains raw payload"
        );
    }
    if u32_at(bytes, 28) & 2 != 0 {
        validated_state(
            [
                f64::from_bits(u64_at(bytes, 64)),
                f64::from_bits(u64_at(bytes, 72)),
            ],
            u64_at(bytes, 80),
            f64::from_bits(u64_at(bytes, 88)),
            f64::from_bits(u64_at(bytes, 96)),
            cells,
        )?;
    } else {
        ensure!(
            bytes[64..104].iter().all(|&b| b == 0),
            "SKV absent summary has nonzero state"
        );
    }
    ensure!(
        !header.summaries || u32_at(bytes, 28) & 2 != 0,
        "SKV declared summary missing"
    );
    Ok(())
}
fn payload_hash(
    header: &Header,
    id: usize,
    record: &[u8; RECORD],
    encoded: &[u8],
) -> Result<blake3::Hash> {
    let mut hash = blake3::Hasher::new();
    if header.grouped() {
        let (leader, count) = header.canonical_group(id)?;
        hash.update(b"SKV-row-group-v1\0");
        hash.update(&(leader as u64).to_le_bytes());
        hash.update(&(count as u32).to_le_bytes());
        hash.update(&record[12..24]);
    } else {
        hash.update(&(id as u64).to_le_bytes());
    }
    hash.update(encoded);
    Ok(hash.finalize())
}
enum WindowOutput {
    Raw(RawWindow),
    Native(Raster),
}
impl SkvSource {
    fn read_window(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        native: bool,
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(WindowOutput, ReadMetrics)> {
        self.guarded(|| {
            check_cancel(cancel)?;
            self.state.borrow().store.verify_local()?;
            let cells = self.validate_window(x, y, w, h, bands)?;
            ensure!(
                self.raw_read_buffer_bound(w, h, bands)? <= max_bytes,
                "SKV raw window exceeds memory budget"
            );
            let start = Instant::now();
            self.prepare_native_window([x, y, w, h], bands, cancel)?;
            let mut output = if native {
                WindowOutput::Native(native_window::allocate(
                    &self.metadata,
                    &self.raw_metadata,
                    [x, y, w, h],
                    bands,
                    cancel,
                )?)
            } else {
                WindowOutput::Raw(RawWindow {
                    width: w,
                    height: h,
                    bands: bands
                        .iter()
                        .map(|&b| RawBandWindow {
                            samples_le: vec![
                                0;
                                cells
                                    * self.raw_metadata.bands[b]
                                        .scalar_type
                                        .byte_width()
                            ],
                            mask: vec![0; cells],
                        })
                        .collect(),
                })
            };
            let mut normalization_ms = 0.;
            let edge = self.header.chunk_edge;
            let nx = self.header.grid.width.div_ceil(edge);
            let mut calls = 0;
            struct Pending {
                out_index: usize,
                band: usize,
                id: usize,
                bounds: [usize; 4],
                record: [u8; RECORD],
            }
            const MAX_PENDING: usize = 256;
            // The queue and the current tile's at most64 records coexist.
            // On64-bit targets320 *184 =58,880 bytes, bounded by64KiB.
            // Within the existing8MiB raw scratch: encoded range<=4MiB,
            // one decoded chunk<=576KiB, predictor<=512KiB, plus bounded
            // codec/HTTP/page scratch. No decoded tile is retained in this queue.
            const _: () = assert!(std::mem::size_of::<Pending>() * (MAX_PENDING + 64) <= 65_536);
            // Conservative grouped allowance: encoded span + decoded packet +
            // historical band/inverse reserves +512KiB bounded codec,
            // transport, queue and structural scratch. Transport cache capacity
            // and the directory LRU are separately retained-reserved. Neither
            // a second packet nor an unbounded compression context is retained.
            // Fused restoration now writes directly to the admitted output and
            // allocates neither historical per-band buffer; keep the cap fixed.
            const _: () = assert!(
                2 * group::MAX_BYTES + 256 * 256 * 9 + MAX_PREDICTOR_SCRATCH + (512 << 10)
                    <= 8 << 20
            );
            let mut limit = self.state.borrow().store.raw_range_limit();
            if self.header.grouped() {
                limit = limit.min(group::MAX_BYTES);
            }
            let mut pending = Vec::<Pending>::with_capacity(MAX_PENDING);
            let mut offset = 0;
            let mut length = 0usize;
            let mut flush = |pending: &mut Vec<Pending>, offset, length| -> Result<()> {
                if pending.is_empty() {
                    return Ok(());
                }
                check_cancel(cancel)?;
                let encoded = self
                    .state
                    .borrow_mut()
                    .read("raw", offset, length, cancel)?;
                calls += 1;
                let physical_count = pending
                    .iter()
                    .enumerate()
                    .filter(|(i, e)| {
                        *i == 0 || u64_at(&pending[*i - 1].record, 0) != u64_at(&e.record, 0)
                    })
                    .count();
                if physical_count > 1 {
                    let mut state = self.state.borrow_mut();
                    state.metrics.coalesced_raw_ranges += 1;
                    state.metrics.coalesced_raw_chunks += physical_count;
                }
                let consume =
                    |entry: &Pending, decoded: &[u8], output: &mut WindowOutput| -> Result<f64> {
                        let [x0, y0, x1, y1] = entry.bounds;
                        let tw = x1 - x0;
                        let (rx0, ry0, rx1, ry1) =
                            (x.max(x0), y.max(y0), (x + w).min(x1), (y + h).min(y1));
                        let info = &self.raw_metadata.bands[entry.band];
                        let scalar = info.scalar_type.byte_width();
                        let sample_bytes = u64_at(&entry.record, 16) as usize;
                        let start = Instant::now();
                        for row in ry0..ry1 {
                            let input = (row - y0) * tw + rx0 - x0;
                            let target = (row - y) * w + rx0 - x;
                            let count = rx1 - rx0;
                            let samples = &decoded[input * scalar..(input + count) * scalar];
                            let mask = &decoded[sample_bytes + input..sample_bytes + input + count];
                            match output {
                                WindowOutput::Raw(out) => {
                                    let out = &mut out.bands[entry.out_index];
                                    out.samples_le[target * scalar..(target + count) * scalar]
                                        .copy_from_slice(samples);
                                    out.mask[target..target + count].copy_from_slice(mask);
                                    self.state.borrow_mut().metrics.copied_bytes +=
                                        (count * (scalar + 1)) as u64;
                                }
                                WindowOutput::Native(out) => {
                                    let out = &mut out.bands[entry.out_index];
                                    native_window::normalize_row(
                                        samples,
                                        mask,
                                        info,
                                        &mut out.values[target..target + count],
                                        &mut out.valid[target..target + count],
                                        cancel,
                                    )?;
                                }
                            }
                        }
                        Ok(if matches!(output, WindowOutput::Native(_)) {
                            start.elapsed().as_secs_f64() * 1000.
                        } else {
                            0.
                        })
                    };
                let mut first = 0;
                while first < pending.len() {
                    let entry = &pending[first];
                    let at = (u64_at(&entry.record, 0) - offset) as usize;
                    let encoded = &encoded[at..at + u32_at(&entry.record, 8) as usize];
                    if self.header.grouped() {
                        let leader = self.header.canonical_group(entry.id)?.0;
                        let end = first
                            + pending[first..]
                                .iter()
                                .take_while(|e| u64_at(&e.record, 112) == leader as u64)
                                .count();
                        ensure!(end > first, "SKV pending group membership mismatch");
                        let packed =
                            self.decode_packet(entry.id, &entry.record, encoded, cancel)?;
                        for member in &pending[first..end] {
                            let [x0, y0, x1, y1] = member.bounds;
                            let (rx0, ry0, rx1, ry1) =
                                (x.max(x0), y.max(y0), (x + w).min(x1), (y + h).min(y1));
                            let (_, count) = self.header.canonical_group(member.id)?;
                            let scalar = self.raw_metadata.bands[member.band]
                                .scalar_type
                                .byte_width();
                            let time = Instant::now();
                            let region = group::CopyRegion {
                                source: [rx0 - x0, ry0 - y0, rx1 - x0, ry1 - y0],
                                target: [rx0 - x, ry0 - y],
                                output_shape: [w, h],
                            };
                            let copied = match &mut output {
                                WindowOutput::Raw(out) => group::restore_member_window(
                                    &packed,
                                    member.id - leader,
                                    count,
                                    x1 - x0,
                                    y1 - y0,
                                    scalar,
                                    self.header.predictor == "byte_delta_v1",
                                    region,
                                    &mut out.bands[member.out_index],
                                    cancel,
                                )?,
                                WindowOutput::Native(out) => {
                                    let copied = group::restore_member_native_window(
                                        &packed,
                                        member.id - leader,
                                        count,
                                        x1 - x0,
                                        y1 - y0,
                                        &self.raw_metadata.bands[member.band],
                                        self.header.predictor == "byte_delta_v1",
                                        region,
                                        &mut out.bands[member.out_index],
                                        cancel,
                                    )?;
                                    normalization_ms += time.elapsed().as_secs_f64() * 1000.;
                                    self.state
                                        .borrow_mut()
                                        .metrics
                                        .native_row_scratch_peak_bytes = 256 * 9;
                                    copied
                                }
                            };
                            let mut state = self.state.borrow_mut();
                            state.metrics.group_restore_ms += time.elapsed().as_secs_f64() * 1000.;
                            state.metrics.copied_bytes += copied as u64;
                        }
                        first = end;
                    } else {
                        let decoded =
                            self.decode_payload(entry.id, &entry.record, encoded, cancel)?;
                        normalization_ms += consume(entry, &decoded, &mut output)?;
                        first += 1;
                    }
                }
                pending.clear();
                Ok(())
            };
            for ty in y / edge..=(y + h - 1) / edge {
                for tx in x / edge..=(x + w - 1) / edge {
                    let tile = ty * nx + tx;
                    let bounds = self.bounds(tile)?;
                    // Gather only records this read already needs. Sorting their
                    // physical offsets changes I/O order, never band output order
                    // or reduction order. There are at most64 selected records.
                    let mut selected = Vec::with_capacity(bands.len());
                    for (out_index, &band) in bands.iter().enumerate() {
                        let id = tile * self.header.bands() + self.mapping[band];
                        selected.push(Pending {
                            out_index,
                            band,
                            id,
                            bounds,
                            record: self.record(id, cancel)?,
                        });
                    }
                    selected.sort_unstable_by_key(|entry| u64_at(&entry.record, 0));
                    let mut selected = selected.into_iter();
                    while let Some(entry) = selected.next() {
                        let next_offset = u64_at(&entry.record, 0);
                        let next_length = u32_at(&entry.record, 8) as usize;
                        // Reserve the whole selected canonical group before
                        // appending aliases, so queue pressure cannot split it
                        // into repeated reads/decodes within this source call.
                        let members = if self.header.grouped() {
                            let leader = u64_at(&entry.record, 112);
                            1 + selected
                                .as_slice()
                                .iter()
                                .take_while(|e| u64_at(&e.record, 112) == leader)
                                .count()
                        } else {
                            1
                        };
                        if !pending.is_empty()
                            && (next_offset != offset + length as u64
                                || length.saturating_add(next_length) > limit
                                || pending.len() + members > MAX_PENDING)
                        {
                            flush(&mut pending, offset, length)?;
                        }
                        ensure!(
                            next_length <= limit,
                            "SKV payload exceeds per-request byte budget (configured range limit)"
                        );
                        if pending.is_empty() {
                            offset = next_offset;
                            length = 0;
                        }
                        length += next_length;
                        pending.push(entry);
                        for _ in 1..members {
                            pending.push(selected.next().expect("counted group member"));
                        }
                    }
                }
            }
            flush(&mut pending, offset, length)?;
            check_cancel(cancel)?;
            self.state.borrow().store.verify_local()?;
            Ok((
                output,
                ReadMetrics {
                    read_decode_ms: (start.elapsed().as_secs_f64() * 1000. - normalization_ms)
                        .max(0.),
                    normalization_ms,
                    raster_io_calls: calls,
                    ..Default::default()
                },
            ))
        })
    }
}
impl WindowSource for SkvSource {
    fn boundary_prefetch_enabled(&self) -> bool {
        self.boundary_read_ahead
            && self.summaries
            && matches!(
                &self.state.borrow().store,
                Store::Remote(source) if source.cache_byte_capacity() >= 4096
            )
    }
    fn prefetch_boundary_windows(
        &self,
        windows: &[[usize; 4]],
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.prepare_boundary_pair(windows, bands, max_bytes, cancel)
    }
    fn max_read_bands(&self) -> usize {
        64
    }
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        Some(&self.raw_metadata)
    }
    fn verify_immutable(&self) -> Result<()> {
        self.guarded(|| self.state.borrow_mut().store.verify())
    }
    fn begin_query_budget(&self) -> Result<()> {
        self.guarded(|| match &mut self.state.borrow_mut().store {
            Store::Remote(source) => source.begin_query_budget(),
            store => store.verify_local(),
        })
    }
    fn begin_verified_query(&self, renew: bool) -> Result<()> {
        self.guarded(|| match &mut self.state.borrow_mut().store {
            Store::Remote(source) => source.begin_verified_query(renew),
            store => store.verify_local(),
        })
    }
    fn end_verified_query(&self) -> Result<()> {
        self.guarded(|| match &mut self.state.borrow_mut().store {
            Store::Remote(source) => source.end_verified_query(),
            store => store.verify_local(),
        })
    }
    fn stored_summaries(&self) -> Option<&dyn StoredSummarySource> {
        self.summaries.then_some(self)
    }
    fn registered_http_identity(&self) -> Option<crate::io::RemoteIdentity> {
        match &self.state.borrow().store {
            Store::Remote(source) => Some(source.registered_identity()),
            _ => None,
        }
    }
    fn retained_memory_bound(&self) -> usize {
        self.retained
    }
    fn identity_descriptor(&self) -> Option<Value> {
        Some(self.descriptor.clone())
    }
    fn access_layout(&self) -> Value {
        json!({"format":"skv","version":0,"physical_hint":if self.header.grouped(){"grouped_typed_row_packets"}else{"independent_typed_band_chunks"},
        "chunk_edge":self.header.chunk_edge,"band_group":self.header.band_group,"payload_layout":self.header.payload_layout,
        "maximum_group_bytes":if self.header.grouped(){Some(group::MAX_BYTES)}else{None},
        "codec":self.header.codec,"predictor":self.header.predictor,
        "deflate_decoder":if cfg!(target_os="linux"){"system_libdeflate"}else{"miniz_oxide"},
        "libdeflate_context_bound_bytes":if cfg!(target_os="linux"){65_536}else{0},
        "source_overview":self.raw_metadata.source_overview,"max_read_bands":self.max_read_bands(),
        "summaries_available":self.summaries,"boundary_read_ahead_enabled":self.boundary_prefetch_enabled(),"self_contained":true,"summary_disabled_reason":self.header.summary_disabled_reason})
    }
    fn diagnostics(&self) -> Value {
        let state = self.state.borrow();
        json!({"format":"skv","metrics":state.metrics,"transport":state.store.diagnostics(),
            "remote":state.store.remote_metrics(),
            "metrics_scope":"source_handle_lifetime","boundary_read_ahead_enabled":self.boundary_prefetch_enabled(),"retained_bound_bytes":self.retained,"invalidated":self.invalidated.get(),
            "trace_truncated":state.metrics.logical_reads>state.metrics.records.len(),"original_source_opened":false})
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        ensure!(
            !bands.is_empty() && bands.len() <= self.max_read_bands(),
            "SKV read band budget exceeded"
        );
        let per_cell = bands.iter().try_fold(0usize, |n, &b| {
            Ok::<_, anyhow::Error>(
                n + self
                    .raw_metadata
                    .bands
                    .get(b)
                    .context("SKV band out of bounds")?
                    .scalar_type
                    .byte_width()
                    + 1,
            )
        })?;
        w.checked_mul(h)
            .and_then(|n| n.checked_mul(per_cell))
            .and_then(|n| n.checked_add(8 << 20))
            .context("SKV read bound overflow")
    }
    fn read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        self.raw_read_buffer_bound(w, h, bands)?
            .checked_add(
                w.checked_mul(h)
                    .and_then(|n| n.checked_mul(bands.len() * 9))
                    .context("SKV normalization size overflow")?,
            )
            .and_then(|n| n.checked_add(crate::source::source_window_overhead(bands.len())))
            .context("SKV window buffer overflow")
    }
    fn read_raw_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(RawWindow, ReadMetrics)> {
        let (output, metrics) = self.read_window(x, y, w, h, bands, false, max_bytes, cancel)?;
        let WindowOutput::Raw(raw) = output else {
            unreachable!("raw output requested")
        };
        Ok((raw, metrics))
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        self.guarded(|| {
            self.state.borrow_mut().metrics.native_window_calls += 1;
            ensure!(
                self.read_buffer_bound(w, h, bands)? <= max_bytes,
                "SKV normalized window exceeds memory budget"
            );
            let (output, metrics) = self.read_window(x, y, w, h, bands, true, max_bytes, cancel)?;
            let WindowOutput::Native(normalized) = output else {
                unreachable!("native output requested")
            };
            {
                let mut state = self.state.borrow_mut();
                let cells = normalized
                    .bands
                    .iter()
                    .map(|band| band.valid.len() as u64)
                    .sum::<u64>();
                state.metrics.native_window_cells += cells;
                state.metrics.native_normalized_written_bytes += cells * 9;
                state.metrics.normalization_ms += metrics.normalization_ms;
                state.metrics.native_output_allocated_bytes +=
                    (normalized.bytes() - std::mem::size_of::<Raster>()) as u64;
            }
            let validation_start = Instant::now();
            let validation = crate::source::validate_window_structure(
                &self.metadata,
                &normalized,
                [x, y, w, h],
                bands,
            );
            let validation_ms = validation_start.elapsed().as_secs_f64() * 1000.;
            self.state.borrow_mut().metrics.native_window_validation_ms += validation_ms;
            validation?;
            Ok((normalized, metrics))
        })
    }
}
impl StoredSummarySource for SkvSource {
    fn summary_layout(&self) -> &SummaryLayout {
        &self.layout
    }
    fn summary_read_buffer_bound(&self, bands: &[usize]) -> Result<usize> {
        ensure!(
            !bands.is_empty() && bands.len() <= 20,
            "SKV summary band budget exceeded"
        );
        // Up to8 remote pages plus one page copy; retained cache capacity is
        // charged separately. Include record/state and HTTP transport scratch.
        let pages = self.state.borrow().store.directory_range_pages().max(1);
        Ok(PAGE * (pages + 1)
            + crate::io::HTTP_SCRATCH_BYTES
            + RECORD
            + bands.len() * std::mem::size_of::<TileSummary>())
    }
    fn read_summary(
        &self,
        node: usize,
        bands: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Vec<TileSummary>> {
        self.guarded(|| {
            ensure!(self.summaries, "SKV stored summaries unavailable");
            check_cancel(cancel)?;
            self.state.borrow().store.verify_local()?;
            self.summary_read_buffer_bound(bands)?;
            let [x0, y0, x1, y1] = self.bounds(node)?;
            let cells = (x1 - x0) * (y1 - y0);
            let mut result = Vec::with_capacity(bands.len());
            for &band in bands {
                let stored = *self
                    .mapping
                    .get(band)
                    .context("SKV summary band out of bounds")?;
                let record = self.record(node * self.header.bands() + stored, cancel)?;
                result.push(validated_state(
                    [
                        f64::from_bits(u64_at(&record, 64)),
                        f64::from_bits(u64_at(&record, 72)),
                    ],
                    u64_at(&record, 80),
                    f64::from_bits(u64_at(&record, 88)),
                    f64::from_bits(u64_at(&record, 96)),
                    cells,
                )?);
            }
            self.state.borrow_mut().metrics.summary_states_read += bands.len();
            check_cancel(cancel)?;
            self.state.borrow().store.verify_local()?;
            Ok(result)
        })
    }
}

fn blank_record(id: usize, leaf: bool) -> [u8; RECORD] {
    let mut bytes = [0; RECORD];
    put_u32(&mut bytes, 28, u32::from(leaf));
    put_u64(&mut bytes, 104, id as u64);
    bytes
}
fn state_of(record: &[u8; RECORD], cells: usize) -> Result<Option<TileSummary>> {
    if u32_at(record, 28) & 2 == 0 {
        return Ok(None);
    }
    Ok(Some(validated_state(
        [
            f64::from_bits(u64_at(record, 64)),
            f64::from_bits(u64_at(record, 72)),
        ],
        u64_at(record, 80),
        f64::from_bits(u64_at(record, 88)),
        f64::from_bits(u64_at(record, 96)),
        cells,
    )?))
}
fn write_state(record: &mut [u8; RECORD], state: &TileSummary) -> Result<()> {
    let min = if state.valid_count == 0 {
        0.
    } else {
        state.min
    };
    let max = if state.valid_count == 0 {
        0.
    } else {
        state.max
    };
    validated_state(
        state.sum.parts(),
        state.valid_count as u64,
        min,
        max,
        state.valid_count,
    )?;
    let flags = u32_at(record, 28) | 2;
    put_u32(record, 28, flags);
    put_u64(record, 64, state.sum.parts()[0].to_bits());
    put_u64(record, 72, state.sum.parts()[1].to_bits());
    put_u64(record, 80, state.valid_count as u64);
    put_u64(record, 88, min.to_bits());
    put_u64(record, 96, max.to_bits());
    Ok(())
}
fn summarize(raster: &Raster, cancel: &AtomicBool) -> Result<TileSummary> {
    ensure!(
        raster.bands.len() == 1,
        "SKV summary expects a single typed band"
    );
    let band = &raster.bands[0];
    let mut state = TileSummary {
        sum: Sum::default(),
        valid_count: 0,
        min: f64::INFINITY,
        max: f64::NEG_INFINITY,
    };
    for (i, (&value, &valid)) in band.values.iter().zip(&band.valid).enumerate() {
        if i % 4096 == 0 {
            check_cancel(cancel)?;
        }
        if valid {
            state.sum.add(value);
            state.valid_count += 1;
            state.min = state.min.min(value);
            state.max = state.max.max(value);
        }
    }
    Ok(state)
}
fn merge_children(
    header: &Header,
    node: usize,
    band: usize,
    mut read: impl FnMut(usize) -> Result<[u8; RECORD]>,
) -> Result<Option<TileSummary>> {
    let layout = header.layout();
    let (level, &(nx, _, first)) = layout
        .levels
        .iter()
        .enumerate()
        .rev()
        .find(|(_, (_, _, offset))| node >= *offset)
        .context("SKV parent level missing")?;
    ensure!(level > 0, "SKV leaf has no children");
    let local = node - first;
    let (tx, ty) = (local % nx, local / nx);
    let (cnx, cny, child_first) = layout.levels[level - 1];
    let mut result = TileSummary {
        sum: Sum::default(),
        valid_count: 0,
        min: f64::INFINITY,
        max: f64::NEG_INFINITY,
    };
    for dy in 0..2 {
        for dx in 0..2 {
            let (x, y) = (tx * 2 + dx, ty * 2 + dy);
            if x >= cnx || y >= cny {
                continue;
            }
            let child = child_first + y * cnx + x;
            let [x0, y0, x1, y1] = layout.bounds(&header.grid, child)?;
            let Some(state) =
                state_of(&read(child * header.bands() + band)?, (x1 - x0) * (y1 - y0))?
            else {
                return Ok(None);
            };
            result.sum.merge(state.sum);
            result.valid_count = result
                .valid_count
                .checked_add(state.valid_count)
                .context("SKV summary count overflow")?;
            result.min = result.min.min(state.min);
            result.max = result.max.max(state.max);
        }
    }
    Ok(Some(result))
}
fn read_record_file(file: &mut File, id: usize) -> Result<[u8; RECORD]> {
    file.seek(SeekFrom::Start((id * RECORD) as u64))?;
    let mut record = [0; RECORD];
    file.read_exact(&mut record)?;
    Ok(record)
}
fn write_record_file(file: &mut File, id: usize, record: &[u8; RECORD]) -> Result<()> {
    file.seek(SeekFrom::Start((id * RECORD) as u64))?;
    file.write_all(record)?;
    Ok(())
}
fn verify_complete(source: &SkvSource, check_logical: bool, cancel: &AtomicBool) -> Result<Value> {
    // A local exhaustive audit is a finite preparation operation, not ordinary
    // serving. It may visit every descriptor/payload, while retaining the same
    // bounded trace/page caches. Remote verification keeps all transport limits.
    let prior_limit = source.state.borrow().logical_read_limit;
    if matches!(source.state.borrow().store, Store::Local { .. }) {
        // Each record can read itself, its canonical group leader, and four
        // children (each possibly resolving a leader). Reserve ten descriptor
        // calls, each loading at most DIRECTORY_RANGE_PAGES pages, plus payload.
        let extra = source
            .header
            .records()?
            .checked_mul(10 * DIRECTORY_RANGE_PAGES + 1)
            .and_then(|n| n.checked_add(source.header.pages().ok()?))
            .context("SKV verification read budget overflow")?;
        let mut state = source.state.borrow_mut();
        state.logical_read_limit = state
            .metrics
            .logical_reads
            .checked_add(extra)
            .context("SKV verification read budget overflow")?
            .max(prior_limit);
    }
    let prior_exhaustive = source.state.borrow().exhaustive_ordered_verify;
    source.state.borrow_mut().exhaustive_ordered_verify = source.header.ordered_large_payloads();
    let result = verify_complete_inner(source, check_logical, cancel);
    source.state.borrow_mut().exhaustive_ordered_verify = prior_exhaustive;
    source.state.borrow_mut().logical_read_limit = prior_limit;
    result
}
fn verify_complete_inner(
    source: &SkvSource,
    check_logical: bool,
    cancel: &AtomicBool,
) -> Result<Value> {
    // Full inspection is explicit. Ordinary serving verifies only visited pages
    // and payloads and does not pretend that a header hash authenticates all data.
    let started = Instant::now();
    source.verify_immutable()?;
    let header = &source.header;
    let mut directory = blake3::Hasher::new();
    for page_id in 0..header.pages()? {
        let page = source.state.borrow_mut().read(
            "directory",
            BOOTSTRAP as u64 + page_id as u64 * PAGE as u64,
            PAGE,
            cancel,
        )?;
        validate_page(header, page_id, &page)?;
        directory.update(&page);
    }
    ensure!(
        directory.finalize().to_hex().as_str() == header.directory_digest,
        "SKV complete directory digest mismatch"
    );
    let mut logical = blake3::Hasher::new();
    let mut states = 0usize;
    let mut raw_bytes = 0u64;
    let mut verified_group: Option<(usize, Vec<u8>)> = None;
    for id in 0..header.records()? {
        check_cancel(cancel)?;
        let record = source.record(id, cancel)?;
        let node = id / header.bands();
        let band = id % header.bands();
        let [x0, y0, x1, y1] = header.layout().bounds(&header.grid, node)?;
        let cells = (x1 - x0) * (y1 - y0);
        let actual = if node < header.leaves() {
            let decoded = if header.grouped() {
                let (leader, _) = header.canonical_group(id)?;
                if verified_group
                    .as_ref()
                    .is_none_or(|(prior, _)| *prior != leader)
                {
                    // Drop the preceding packet before reading its successor.
                    drop(verified_group.take());
                    let encoded = source.state.borrow_mut().read(
                        "raw",
                        u64_at(&record, 0),
                        u32_at(&record, 8) as usize,
                        cancel,
                    )?;
                    let packed = source.decode_packet(id, &record, &encoded, cancel)?;
                    verified_group = Some((leader, packed));
                }
                source.group_member(
                    id,
                    &record,
                    &verified_group.as_ref().expect("group loaded").1,
                    cancel,
                )?
            } else {
                source.payload(id, &record, cancel)?
            };
            logical.update(&(id as u64).to_le_bytes());
            logical.update(&decoded);
            raw_bytes += decoded.len() as u64;
            if u32_at(&record, 28) & 2 != 0 {
                let sample = u64_at(&record, 16) as usize;
                let raw = RawWindow {
                    width: x1 - x0,
                    height: y1 - y0,
                    bands: vec![RawBandWindow {
                        samples_le: decoded[..sample].to_vec(),
                        mask: decoded[sample..].to_vec(),
                    }],
                };
                // Header-wide metadata is used even when the serving handle has a
                // selected-band view; verify always audits every stored band.
                let metadata = metadata_for_header(header);
                let normalized = raw.normalize(
                    &header.raw_metadata,
                    &metadata,
                    x0,
                    y0,
                    &[band],
                    16 << 20,
                    cancel,
                )?;
                Some(summarize(&normalized, cancel)?)
            } else {
                None
            }
        } else {
            merge_children(header, node, band, |child| source.record(child, cancel))?
        };
        if let Some(stored) = state_of(&record, cells)? {
            let computed = actual.context("SKV summary cannot be recomputed from children")?;
            let mut comparison = blank_record(id, node < header.leaves());
            write_state(&mut comparison, &computed)?;
            ensure!(
                comparison[64..104] == record[64..104],
                "SKV summary does not match raw values or child states"
            );
            ensure!(
                stored.valid_count <= cells,
                "SKV summary count exceeds coverage"
            );
            states += 1;
        }
    }
    let logical_digest = logical.finalize().to_hex().to_string();
    if check_logical {
        ensure!(
            logical_digest == header.logical_digest,
            "SKV complete logical digest mismatch"
        );
    }
    {
        let state = source.state.borrow();
        let mut position = header.data_offset()?;
        if !header.ordered_large_payloads() {
            ensure!(
                state.intervals.len() == header.leaves() * header.groups_per_tile()?,
                "SKV missing typed payload interval"
            );
            for &(begin, end, _) in &state.intervals {
                ensure!(begin == position, "SKV payload gap or overlap");
                position = end;
            }
            ensure!(
                position == state.store.length(),
                "SKV unreferenced trailing payload"
            );
        } else {
            ensure!(
                state.intervals.len() <= MAX_LEAF_RECORDS,
                "SKV ordered visited interval limit exceeded"
            );
        }
    }
    check_cancel(cancel)?;
    source.verify_immutable()?;
    Ok(
        json!({"verified":true,"format":"skv","version":0,"all_stored_bands":header.bands(),
        "leaf_chunks":header.leaves()*header.bands(),"records":header.records()?,"summary_states_verified":states,
        "decoded_raw_bytes":raw_bytes,"logical_digest":logical_digest,"directory_digest":header.directory_digest,
        "root_blake3":source.root_digest,"verification_ms":started.elapsed().as_secs_f64()*1000.,
        "original_source_opened":false,"diagnostics":source.diagnostics()}),
    )
}
fn metadata_for_header(header: &Header) -> RasterMetadata {
    RasterMetadata {
        grid: header.grid.clone(),
        source_id: header.original_source_id.clone(),
        bands: header
            .raw_metadata
            .bands
            .iter()
            .map(|b| BandMetadata {
                data_type: format!("{:?}", b.scalar_type),
                nodata: b.nodata_f64_bits.map(f64::from_bits),
                scale: f64::from_bits(b.scale_f64_bits),
                offset: f64::from_bits(b.offset_f64_bits),
                unit: b.unit.clone(),
                block_size: (header.chunk_edge, header.chunk_edge),
            })
            .collect(),
    }
}
/// Explicit complete verification reads every directory record and payload. It
/// never follows original-source provenance and is not part of each query open.
pub fn verify(spec: &SourceSpec, cancel: &AtomicBool) -> Result<Value> {
    let source = SkvSource::open(spec, cancel)?;
    verify_complete(&source, true, cancel)
}
struct TemporaryFiles(Vec<PathBuf>);
impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}
static BUILD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Compile exactly the exposed typed source bands. Raw values and masks remain
/// native-width bytes; summaries are optional, derived normalized f64 states.
pub fn compile(
    source: &dyn WindowSource,
    output: &str,
    options: &CompileOptions,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    check_cancel(cancel)?;
    options.validate()?;
    source.verify_immutable()?;
    let raw_metadata = source
        .raw_metadata()
        .context("source does not support lossless typed SKV compilation")?
        .clone();
    ensure!(
        raw_metadata.bands.len() == source.metadata().bands.len(),
        "SKV raw/normalized metadata band mismatch"
    );
    let layout = SummaryLayout::new(&source.metadata().grid, options.chunk_edge, true)?;
    let mut generation = Sha256::new();
    generation.update(source.metadata().source_id.as_bytes());
    generation.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            .to_le_bytes(),
    );
    generation.update(BUILD_SEQUENCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    generation.update(std::process::id().to_le_bytes());
    generation.update(serde_json::to_vec(options)?);
    let build_id = format!("{:x}", generation.finalize());
    let mut header = Header {
        version: 0,
        grid: source.metadata().grid.clone(),
        raw_metadata,
        chunk_edge: options.chunk_edge,
        band_group: options.band_group,
        payload_layout: options.payload_layout.clone(),
        codec: options.codec.clone(),
        predictor: options.predictor.clone(),
        compression_level: options.compression_level,
        summaries: options.summaries,
        hierarchy: HierarchyDescription {
            tile_edge: layout.tile_edge,
            levels: layout.levels.clone(),
        },
        numerical_schema: SCHEMA.into(),
        build_id: build_id.clone(),
        original_source_id: source.metadata().source_id.clone(),
        original_identity: source.identity_descriptor(),
        logical_digest: "0".repeat(64),
        directory_digest: "0".repeat(64),
        summary_disabled_reason: None,
    };
    header.validate()?;
    let edge = options.chunk_edge;
    let cells = edge * edge;
    let mut working = 32usize << 20;
    for band in 0..header.bands() {
        let extra = cells
            .checked_mul(3 * (header.raw_metadata.bands[band].scalar_type.byte_width() + 1) + 9)
            .and_then(|n| n.checked_add(4 << 20))
            .context("SKV build buffer overflow")?;
        let extra = if header.grouped() {
            extra
                .checked_add(2 * group::MAX_BYTES + (1 << 20))
                .context("SKV grouped build buffer overflow")?
        } else {
            extra
        };
        // Admission must cover the actual compiler schedule: a source may
        // prove a smaller footprint for an aligned chunk, or require more for
        // a later chunk crossing a physical block boundary. Readers without a
        // position-aware bound retain the conservative default trait contract.
        for node in 0..header.leaves() {
            check_cancel(cancel)?;
            let [x0, y0, x1, y1] = layout.bounds(&header.grid, node)?;
            let input = source.raw_read_buffer_bound_at(x0, y0, x1 - x0, y1 - y0, &[band])?;
            working = working.max(
                input
                    .checked_add(extra)
                    .context("SKV build buffer overflow")?,
            );
        }
    }
    check_cancel(cancel)?;
    ensure!(
        working <= options.working_bytes,
        "SKV compilation exceeds working memory budget: requires {working} bytes"
    );
    ensure!(
        header.data_offset()? < options.max_output_bytes,
        "SKV directory exceeds requested output limit"
    );
    let destination = Path::new(output);
    let name = destination
        .file_name()
        .context("SKV output needs a file name")?
        .to_str()
        .context("non-UTF8 SKV output")?;
    ensure!(
        !name.is_empty() && name != "." && name != "..",
        "invalid SKV output name"
    );
    let parent = fs::canonicalize(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let destination = parent.join(name);
    ensure!(
        !destination.try_exists()?,
        "SKV output already exists; compilation never overwrites"
    );
    let temporary = parent.join(format!(".{name}.{}.incomplete", &build_id[..20]));
    let record_path = parent.join(format!(".{name}.{}.records", &build_id[..20]));
    let mut owned = TemporaryFiles(Vec::new());
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    owned.0.push(temporary.clone());
    let mut records = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&record_path)?;
    owned.0.push(record_path);
    file.set_len(header.data_offset()?)?;
    file.seek(SeekFrom::Start(header.data_offset()?))?;
    records.set_len((header.records()? * RECORD) as u64)?;
    let mut position = header.data_offset()?;
    let mut sample_bytes = 0u64;
    let mut mask_bytes = 0u64;
    let mut source_calls = 0usize;
    let mut source_read_ms = 0.;
    let mut compression_ms = 0.;
    let mut group_pack_ms = 0.;
    let mut predictor_encode_ms = 0.;
    let mut predictor_scratch_peak_bytes = 0;
    let mut summary_ms = 0.;
    let codec = u32::from(options.codec == "deflate");
    let mut all_summaries = options.summaries;
    // Independent-band storage keeps its historical physical order. Optional
    // row groups share one packet only within deterministic, bounded members.
    let mut physical_groups = Vec::new();
    let mut first = 0;
    while first < header.bands() {
        let count = if header.grouped() {
            header.band_group_bounds(first)?.1
        } else {
            options.band_group.min(header.bands() - first)
        };
        physical_groups.push((first, count));
        first += count;
    }
    for (first, group_count) in physical_groups {
        for node in 0..header.leaves() {
            let [x0, y0, x1, y1] = layout.bounds(&header.grid, node)?;
            let mut group_packet = if header.grouped() {
                let scalar = header.raw_metadata.bands[first].scalar_type.byte_width();
                let (_, band_bytes) = group::shape(x1 - x0, y1 - y0, scalar, group_count)?;
                vec![0; band_bytes * group_count]
            } else {
                Vec::new()
            };
            let mut group_records =
                Vec::with_capacity(if header.grouped() { group_count } else { 0 });
            for band in first..first + group_count {
                check_cancel(cancel)?;
                let id = node * header.bands() + band;
                let (raw, metrics) = source.read_raw_selected_window_cancellable(
                    x0,
                    y0,
                    x1 - x0,
                    y1 - y0,
                    &[band],
                    options.working_bytes,
                    cancel,
                )?;
                source_calls += metrics.raster_io_calls;
                source_read_ms += metrics.read_decode_ms;
                ensure!(
                    raw.width == x1 - x0 && raw.height == y1 - y0 && raw.bands.len() == 1,
                    "source returned wrong raw chunk shape"
                );
                let count = (x1 - x0) * (y1 - y0);
                let samples = count * header.raw_metadata.bands[band].scalar_type.byte_width();
                ensure!(
                    raw.bands[0].samples_le.len() == samples && raw.bands[0].mask.len() == count,
                    "source returned wrong raw chunk bytes"
                );
                let mut record = blank_record(id, true);
                if options.summaries {
                    let time = Instant::now();
                    let normalized = raw.normalize(
                        &header.raw_metadata,
                        source.metadata(),
                        x0,
                        y0,
                        &[band],
                        options.working_bytes,
                        cancel,
                    )?;
                    let state = summarize(&normalized, cancel)?;
                    if write_state(&mut record, &state).is_err() {
                        all_summaries = false;
                    }
                    summary_ms += time.elapsed().as_secs_f64() * 1000.;
                }
                let mut payload = Vec::with_capacity(samples + count);
                payload.extend_from_slice(&raw.bands[0].samples_le);
                payload.extend_from_slice(&raw.bands[0].mask);
                if options.predictor == "byte_delta_v1" {
                    let time = Instant::now();
                    let scratch = byte_delta(
                        &mut payload,
                        samples,
                        x1 - x0,
                        y1 - y0,
                        header.raw_metadata.bands[band].scalar_type.byte_width(),
                        false,
                        cancel,
                    )?;
                    predictor_encode_ms += time.elapsed().as_secs_f64() * 1000.;
                    predictor_scratch_peak_bytes = predictor_scratch_peak_bytes.max(scratch);
                }
                if header.grouped() {
                    let time = Instant::now();
                    group::insert(
                        &mut group_packet,
                        &payload,
                        band - first,
                        group_count,
                        x1 - x0,
                        y1 - y0,
                        header.raw_metadata.bands[band].scalar_type.byte_width(),
                        header.predictor == "byte_delta_v1",
                        cancel,
                    )?;
                    group_pack_ms += time.elapsed().as_secs_f64() * 1000.;
                    group_records.push(record);
                    sample_bytes += samples as u64;
                    mask_bytes += count as u64;
                    continue;
                }
                let time = Instant::now();
                let encoded = encode(&payload, codec, options.compression_level, cancel)?;
                compression_ms += time.elapsed().as_secs_f64() * 1000.;
                ensure!(
                    !encoded.is_empty() && encoded.len() <= 4 << 20,
                    "SKV encoded chunk exceeds limit"
                );
                let end = position
                    .checked_add(encoded.len() as u64)
                    .context("SKV output size overflow")?;
                ensure!(
                    end <= options.max_output_bytes,
                    "SKV compilation exceeds requested output byte limit"
                );
                put_u64(&mut record, 0, position);
                put_u32(&mut record, 8, encoded.len() as u32);
                put_u32(&mut record, 12, payload.len() as u32);
                put_u64(&mut record, 16, samples as u64);
                put_u32(&mut record, 24, codec);
                let mut hash = blake3::Hasher::new();
                hash.update(&(id as u64).to_le_bytes());
                hash.update(&encoded);
                record[32..64].copy_from_slice(hash.finalize().as_bytes());
                file.write_all(&encoded)?;
                write_record_file(&mut records, id, &record)?;
                position = end;
                sample_bytes += samples as u64;
                mask_bytes += count as u64;
            }
            if header.grouped() {
                let time = Instant::now();
                let (encoded, stored_codec) =
                    group::encode(&group_packet, codec, options.compression_level, cancel)?;
                compression_ms += time.elapsed().as_secs_f64() * 1000.;
                let end = position
                    .checked_add(encoded.len() as u64)
                    .context("SKV output size overflow")?;
                ensure!(
                    end <= options.max_output_bytes,
                    "SKV compilation exceeds requested output byte limit"
                );
                let leader = node * header.bands() + first;
                let count = (x1 - x0) * (y1 - y0);
                let mut group_digest = None;
                for (member, mut record) in group_records.into_iter().enumerate() {
                    let id = leader + member;
                    put_u64(&mut record, 0, position);
                    put_u32(&mut record, 8, encoded.len() as u32);
                    put_u32(&mut record, 12, group_packet.len() as u32);
                    put_u64(
                        &mut record,
                        16,
                        (count * header.raw_metadata.bands[first].scalar_type.byte_width()) as u64,
                    );
                    put_u32(&mut record, 24, stored_codec);
                    put_u64(&mut record, 112, leader as u64);
                    put_u32(&mut record, 120, group_count as u32);
                    // The homogeneous members have identical hashed shape and
                    // canonical identity; hash encoded bytes once per packet.
                    let digest = match group_digest {
                        Some(digest) => digest,
                        None => {
                            let digest = payload_hash(&header, id, &record, &encoded)?;
                            group_digest = Some(digest);
                            digest
                        }
                    };
                    record[32..64].copy_from_slice(digest.as_bytes());
                    write_record_file(&mut records, id, &record)?;
                }
                file.write_all(&encoded)?;
                position = end;
            }
        }
    }
    for node in header.leaves()..layout.records()? {
        for band in 0..header.bands() {
            check_cancel(cancel)?;
            let id = node * header.bands() + band;
            let mut record = blank_record(id, false);
            if options.summaries {
                let time = Instant::now();
                match merge_children(&header, node, band, |child| {
                    read_record_file(&mut records, child)
                })? {
                    Some(state) => {
                        if write_state(&mut record, &state).is_err() {
                            all_summaries = false;
                        }
                    }
                    None => all_summaries = false,
                }
                summary_ms += time.elapsed().as_secs_f64() * 1000.;
            }
            write_record_file(&mut records, id, &record)?;
        }
    }
    header.summaries = all_summaries;
    if options.summaries && !all_summaries {
        header.summary_disabled_reason =
            Some("nonfinite_or_unrepresentable_compensated_state".into());
    }
    let assemble = Instant::now();
    let mut directory = blake3::Hasher::new();
    file.seek(SeekFrom::Start(BOOTSTRAP as u64))?;
    records.seek(SeekFrom::Start(0))?;
    for page_id in 0..header.pages()? {
        check_cancel(cancel)?;
        let count = (header.records()? - page_id * RECORDS_PER_PAGE).min(RECORDS_PER_PAGE);
        let mut page = vec![0; PAGE];
        page[..4].copy_from_slice(b"SKVP");
        page[6..8].copy_from_slice(&(count as u16).to_le_bytes());
        put_u64(&mut page, 8, page_id as u64);
        records.read_exact(&mut page[16..16 + count * RECORD])?;
        let checksum = blake3::hash(&page[..PAGE - 32]);
        page[PAGE - 32..].copy_from_slice(checksum.as_bytes());
        directory.update(&page);
        file.write_all(&page)?;
    }
    header.directory_digest = directory.finalize().to_hex().to_string();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&encode_header(&header, position, cancel)?)?;
    file.flush()?;
    let assembly_ms = assemble.elapsed().as_secs_f64() * 1000.;
    let temp_spec: SourceSpec = serde_json::from_value(
        json!({"location":temporary.to_str().context("non-UTF8 SKV temporary path")?,"format":"skv"}),
    )?;
    let verifier = SkvSource::open(&temp_spec, cancel)?;
    let verification = verify_complete(&verifier, false, cancel)?;
    header.logical_digest = verification["logical_digest"]
        .as_str()
        .context("missing SKV verifier digest")?
        .into();
    drop(verifier);
    let bootstrap = encode_header(&header, position, cancel)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bootstrap)?;
    file.sync_all()?;
    // Report a portable content identity, charging this complete pass to build
    // cost. Queries only rehash when the caller explicitly requests verification.
    let hash_time = Instant::now();
    file.seek(SeekFrom::Start(0))?;
    let mut sha = Sha256::new();
    let mut remaining = position;
    // A later hash pass must not reserve64KiB in this entire function's stack
    // frame, including earlier calls into the compression backend.
    let mut buffer = vec![0u8; 65_536];
    while remaining > 0 {
        check_cancel(cancel)?;
        let n = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n])?;
        sha.update(&buffer[..n]);
        remaining -= n as u64;
    }
    let sha256 = format!("{:x}", sha.finalize());
    let content_hash_ms = hash_time.elapsed().as_secs_f64() * 1000.;
    source.verify_immutable()?;
    check_cancel(cancel)?;
    // hard_link is an atomic no-replace publication on the same filesystem.
    // Unsupported filesystems fail closed and retain no final destination.
    fs::hard_link(&temporary, &destination)
        .context("cannot atomically create SKV destination without replacement")?;
    let directory_sync = File::open(&parent)
        .and_then(|directory| directory.sync_all())
        .is_ok();
    let mut receipt = json!({"compiled":true,"format":"skv","version":0,"unstable":true,"output":destination,
        "build_id":build_id,"sha256":sha256,"byte_length":position,"logical_digest":header.logical_digest,
        "root_blake3":blake3::hash(&bootstrap).to_hex().to_string(),"directory_digest":header.directory_digest,
        "grid":header.grid,"bands":header.bands(),"summaries":header.summaries,"summary_disabled_reason":header.summary_disabled_reason,
        "sample_bytes":sample_bytes,"mask_bytes":mask_bytes,"directory_bytes":header.pages()?*PAGE,
        "bootstrap_bytes":BOOTSTRAP,"encoded_payload_bytes":position-header.data_offset()?,"leaf_chunks":header.leaves()*header.bands(),
        "working_bound_bytes":working,"source_retained_bytes":source.retained_memory_bound(),
        "peak_scratch_file_bytes":position+(header.records()?*RECORD) as u64,
        "source_raster_io_calls":source_calls,"source_read_decode_ms":source_read_ms,"compression_ms":compression_ms,
        "summary_ms":summary_ms,"directory_assembly_ms":assembly_ms,"content_sha256_ms":content_hash_ms,"content_sha256_read_bytes":position,
        "verification":verification,"source_diagnostics":source.diagnostics(),"source_identity":source.identity_descriptor(),
        "directory_sync_completed":directory_sync,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,
        "serving_requires_original":false,"overwrite":false});
    receipt["codec"] = json!(header.codec);
    receipt["predictor"] = json!(header.predictor);
    receipt["payload_layout"] = json!(header.payload_layout);
    receipt["group_pack_ms"] = json!(group_pack_ms);
    receipt["payload_checksum_scope"] = json!(if header.grouped() {
        "once_per_canonical_group_packet"
    } else {
        "once_per_independent_band_chunk"
    });
    receipt["physical_payloads"] = json!(header.leaves() * header.groups_per_tile()?);
    receipt["maximum_group_bytes"] = if header.grouped() {
        json!(group::MAX_BYTES)
    } else {
        Value::Null
    };
    receipt["predictor_encode_ms"] = json!(predictor_encode_ms);
    receipt["predictor_scratch_peak_bytes"] = json!(predictor_scratch_peak_bytes);
    drop(records);
    drop(file);
    drop(owned);
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
    fn spec(path: &Path) -> SourceSpec {
        serde_json::from_value(json!({"location":path,"format":"skv"})).unwrap()
    }
    fn fixture(path: &Path, huge: bool) {
        let mut dataset = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<f64, _>(path, 129, 131, 1)
            .unwrap();
        dataset
            .set_geo_transform(&[0., 1., 0., 131., 0., -1.])
            .unwrap();
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        let values = (0..129 * 131)
            .map(|i| {
                if huge {
                    f64::MAX
                } else {
                    (i % 197) as f64 - 98.
                }
            })
            .collect();
        dataset
            .rasterband(1)
            .unwrap()
            .write((0, 0), (129, 131), &mut Buffer::new((129, 131), values))
            .unwrap();
        dataset.flush_cache().unwrap();
    }
    fn built(dir: &Path, huge: bool) -> (PathBuf, Value) {
        let input = dir.join("input.tif");
        fixture(&input, huge);
        let input_spec = serde_json::from_value(json!({"location":input})).unwrap();
        let source =
            crate::io::open_source_for_compile(&input_spec, &AtomicBool::new(false)).unwrap();
        let target = dir.join("input.skv");
        let options = CompileOptions {
            chunk_edge: 64,
            ..Default::default()
        };
        let receipt = compile(
            source.as_ref(),
            target.to_str().unwrap(),
            &options,
            &AtomicBool::new(false),
        )
        .unwrap();
        (target, receipt)
    }
    fn bootstrap_hash(bytes: &mut [u8]) {
        let hash = blake3::hash(&bytes[..BOOTSTRAP - 32]);
        bytes[BOOTSTRAP - 32..BOOTSTRAP].copy_from_slice(hash.as_bytes());
    }
    fn mutate_record(bytes: &mut [u8], id: usize, change: impl FnOnce(&mut [u8])) {
        let page_start = BOOTSTRAP + id / RECORDS_PER_PAGE * PAGE;
        let at = page_start + 16 + id % RECORDS_PER_PAGE * RECORD;
        change(&mut bytes[at..at + RECORD]);
        let hash = blake3::hash(&bytes[page_start..page_start + PAGE - 32]);
        bytes[page_start + PAGE - 32..page_start + PAGE].copy_from_slice(hash.as_bytes());
        let mut header = decode_header(
            &bytes[..BOOTSTRAP],
            bytes.len() as u64,
            &AtomicBool::new(false),
        )
        .unwrap();
        header.directory_digest =
            blake3::hash(&bytes[BOOTSTRAP..header.data_offset().unwrap() as usize])
                .to_hex()
                .to_string();
        let encoded = encode_header(&header, bytes.len() as u64, &AtomicBool::new(false)).unwrap();
        bytes[..BOOTSTRAP].copy_from_slice(&encoded);
    }
    #[test]
    fn large_grid_capacity_is_bounded_and_preserves_existing_header_layout() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = built(dir.path(), false);
        let old = SkvSource::open(&spec(&path), &AtomicBool::new(false)).unwrap();
        assert_eq!(old.state.borrow().intervals.capacity(), 9);
        let mut header = old.header.clone();
        header.grid.width = 17643;
        header.grid.height = 11708;
        header.chunk_edge = 256;
        header.band_group = 36;
        header.payload_layout = "row_group_v1".into();
        let mut band = header.raw_metadata.bands[0].clone();
        band.scalar_type = crate::source::RawScalarType::Float32;
        header.raw_metadata.bands = (0..36)
            .map(|i| {
                let mut value = band.clone();
                value.original_band_index = i;
                value
            })
            .collect();
        header.raw_metadata.source_band_count = 36;
        header.hierarchy = HierarchyDescription {
            tile_edge: 256,
            levels: SummaryLayout::new(&header.grid, 256, true).unwrap().levels,
        };
        header.validate().unwrap();
        assert_eq!(header.leaves() * header.bands(), 114264);
        assert_eq!(header.groups_per_tile().unwrap(), 4);
        assert_eq!(header.band_group_bounds(0).unwrap(), (0, 10));
        let large = dir.path().join("large_grid-header.skv");
        let length = header.data_offset().unwrap() + 1;
        let bytes = encode_header(&header, length, &AtomicBool::new(false)).unwrap();
        fs::write(&large, bytes).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(length)
            .unwrap();
        let opened = SkvSource::open(&spec(&large), &AtomicBool::new(false)).unwrap();
        assert_eq!(opened.state.borrow().intervals.capacity(), 12696);
        assert!(opened.retained <= crate::source::NATIVE_SOURCE_RETAINED_BYTES);
        // Capacity extension changes no header version or byte interpretation.
        assert_eq!(opened.header.version, 0);
        header.payload_layout = "band".into();
        let bytes = encode_header(&header, length, &AtomicBool::new(false)).unwrap();
        let independent = dir.path().join("large_grid-band-header.skv");
        fs::write(&independent, bytes).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&independent)
            .unwrap()
            .set_len(length)
            .unwrap();
        let band_source = SkvSource::open(&spec(&independent), &AtomicBool::new(false)).unwrap();
        assert_eq!(band_source.state.borrow().intervals.capacity(), 114264);
        assert!(band_source.retained <= crate::source::NATIVE_SOURCE_RETAINED_BYTES);
        header.chunk_edge = 128;
        header.hierarchy = HierarchyDescription {
            tile_edge: 128,
            levels: SummaryLayout::new(&header.grid, 128, true).unwrap().levels,
        };
        assert!(
            header
                .validate()
                .unwrap_err()
                .to_string()
                .contains("131072")
        );
    }
    // Sparse capacity fixture: only the first directory range is populated.
    // It is deliberately not a complete valid object; tests below prove that
    // accessed descriptors retain fail-closed checks without scanning the file.
    fn sparse_ordered_fixture(dir: &Path, patch: impl Fn(usize, &mut [u8; RECORD])) -> PathBuf {
        let (path, _) = built(dir, false);
        let original = SkvSource::open(&spec(&path), &AtomicBool::new(false)).unwrap();
        let mut header = original.header.clone();
        header.grid.width = 430706;
        header.grid.height = 62971;
        header.chunk_edge = 256;
        header.band_group = 10;
        header.payload_layout = "row_group_v1".into();
        header.summaries = false;
        let mut band = header.raw_metadata.bands[0].clone();
        band.scalar_type = crate::source::RawScalarType::Float32;
        header.raw_metadata.bands = (0..36)
            .map(|i| {
                let mut b = band.clone();
                b.original_band_index = i;
                b
            })
            .collect();
        header.raw_metadata.source_band_count = 36;
        header.hierarchy = HierarchyDescription {
            tile_edge: 256,
            levels: SummaryLayout::new(&header.grid, 256, true).unwrap().levels,
        };
        let length = 64u64 << 30;
        let target = dir.join("sparse-ordered.skv");
        let mut file = File::create(&target).unwrap();
        file.write_all(&encode_header(&header, length, &AtomicBool::new(false)).unwrap())
            .unwrap();
        file.set_len(length).unwrap();
        for page_id in 0..DIRECTORY_RANGE_PAGES {
            let mut page = vec![0u8; PAGE];
            page[..4].copy_from_slice(b"SKVP");
            page[6..8].copy_from_slice(&(RECORDS_PER_PAGE as u16).to_le_bytes());
            put_u64(&mut page, 8, page_id as u64);
            for local in 0..RECORDS_PER_PAGE {
                let id = page_id * RECORDS_PER_PAGE + local;
                let mut record = [0u8; RECORD];
                let (leader, count) = header.canonical_group(id).unwrap();
                put_u64(
                    &mut record,
                    0,
                    header.data_offset().unwrap() + (id / 36) as u64,
                );
                put_u32(&mut record, 8, 1);
                put_u32(&mut record, 12, (256 * 256 * 5 * count) as u32);
                put_u64(&mut record, 16, 256 * 256 * 4);
                put_u32(&mut record, 24, 1);
                put_u32(&mut record, 28, 1);
                put_u64(&mut record, 104, id as u64);
                put_u64(&mut record, 112, leader as u64);
                put_u32(&mut record, 120, count as u32);
                patch(id, &mut record);
                let at = 16 + local * RECORD;
                page[at..at + RECORD].copy_from_slice(&record);
            }
            let digest = blake3::hash(&page[..PAGE - 32]);
            page[PAGE - 32..].copy_from_slice(digest.as_bytes());
            file.write_all(&page).unwrap();
        }
        target
    }
    #[test]
    fn large_ordered_descriptors_reject_gaps_aliases_and_nonlocal_overlap() {
        let cancel = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_ordered_fixture(dir.path(), |_, _| {});
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        source.record(0, &cancel).unwrap();
        source.record(1, &cancel).unwrap();
        source.record(36, &cancel).unwrap();
        assert_eq!(source.state.borrow().intervals.len(), 2);
        assert!(source.state.borrow().metrics.logical_bytes < 1 << 20);
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_ordered_fixture(dir.path(), |id, r| {
            if id == 36 {
                let offset = u64_at(r, 0) + 1;
                put_u64(r, 0, offset);
            }
        });
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        assert!(
            source
                .record(0, &cancel)
                .unwrap_err()
                .to_string()
                .contains("gap or overlap")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_ordered_fixture(dir.path(), |id, r| {
            if id == 1 {
                r[32] = 1;
            }
        });
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        assert!(
            source
                .record(1, &cancel)
                .unwrap_err()
                .to_string()
                .contains("alias differs")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_ordered_fixture(dir.path(), |id, r| {
            if (3..=5).contains(&(id / 36)) {
                let offset = u64_at(r, 0) - 3;
                put_u64(r, 0, offset);
            }
        });
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        source.record(36, &cancel).unwrap();
        // Its immediate neighbors are internally contiguous, but it aliases an
        // already visited non-neighbor: the bounded cross-visited guard catches it.
        assert!(
            source
                .record(144, &cancel)
                .unwrap_err()
                .to_string()
                .contains("overlapping")
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(source.descriptor(400, &cancel).is_err());
    }

    #[test]
    fn large_sparse_native_window_checks_all_bands_over64gib_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = sparse_ordered_fixture(dir.path(), |_, _| {});
        let cancel = AtomicBool::new(false);
        let opened = SkvSource::open(&spec(&path), &cancel).unwrap();
        let header = opened.header.clone();
        drop(opened);
        let base = header.data_offset().unwrap();
        let starts = [base, 16u64 << 30, 32u64 << 30, 48u64 << 30];
        let firsts = [0usize, 10, 20, 30];
        let mut payloads = Vec::new();
        for first in firsts {
            let (_, count) = header.band_group_bounds(first).unwrap();
            payloads.push(
                group::encode(&vec![0u8; 256 * 256 * 5 * count], 1, 3, &cancel)
                    .unwrap()
                    .0,
            );
        }
        let mut ranges = std::collections::BTreeSet::new();
        for id in [
            0,
            10,
            20,
            30,
            (header.leaves() - 1) * 36,
            (header.leaves() - 1) * 36 + 10,
            (header.leaves() - 1) * 36 + 20,
        ] {
            let first = (id / RECORDS_PER_PAGE) / DIRECTORY_RANGE_PAGES * DIRECTORY_RANGE_PAGES;
            for page in first..first + DIRECTORY_RANGE_PAGES {
                ranges.insert(page);
            }
        }
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        for page_id in ranges {
            let mut page = vec![0u8; PAGE];
            page[..4].copy_from_slice(b"SKVP");
            page[6..8].copy_from_slice(&(RECORDS_PER_PAGE as u16).to_le_bytes());
            put_u64(&mut page, 8, page_id as u64);
            for local in 0..RECORDS_PER_PAGE {
                let id = page_id * RECORDS_PER_PAGE + local;
                let node = id / 36;
                if node >= header.leaves() {
                    continue;
                }
                let (leader, count) = header.canonical_group(id).unwrap();
                let group = (leader % 36) / 10;
                let [x0, y0, x1, y1] = header.layout().bounds(&header.grid, node).unwrap();
                let cells = (x1 - x0) * (y1 - y0);
                let mut r = [0u8; RECORD];
                let encoded = if node == 0 { payloads[group].len() } else { 1 };
                let offset = if node == 0 {
                    starts[group]
                } else if node == 1 {
                    starts[group] + payloads[group].len() as u64
                } else if node + 1 == header.leaves() && group < 3 {
                    starts[group + 1] - 1
                } else {
                    starts[group] + payloads[group].len() as u64 + node as u64
                };
                put_u64(&mut r, 0, offset);
                put_u32(&mut r, 8, encoded as u32);
                put_u32(&mut r, 12, (cells * 5 * count) as u32);
                put_u64(&mut r, 16, (cells * 4) as u64);
                put_u32(&mut r, 24, 1);
                put_u32(&mut r, 28, 1);
                put_u64(&mut r, 104, id as u64);
                put_u64(&mut r, 112, leader as u64);
                put_u32(&mut r, 120, count as u32);
                if node == 0 {
                    let hash = payload_hash(&header, id, &r, &payloads[group]).unwrap();
                    r[32..64].copy_from_slice(hash.as_bytes());
                }
                let at = 16 + local * RECORD;
                page[at..at + RECORD].copy_from_slice(&r);
            }
            let hash = blake3::hash(&page[..PAGE - 32]);
            page[PAGE - 32..].copy_from_slice(hash.as_bytes());
            file.seek(SeekFrom::Start(
                BOOTSTRAP as u64 + page_id as u64 * PAGE as u64,
            ))
            .unwrap();
            file.write_all(&page).unwrap();
        }
        for (start, payload) in starts.into_iter().zip(payloads) {
            file.seek(SeekFrom::Start(start)).unwrap();
            file.write_all(&payload).unwrap();
        }
        drop(file);
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        let bands = (0..36).collect::<Vec<_>>();
        let (window, _) = source
            .read_selected_window_cancellable(0, 0, 2, 2, &bands, 128 << 20, &cancel)
            .unwrap();
        assert_eq!(window.bands.len(), 36);
        assert!(window.bands.iter().all(|b| b.valid == vec![false; 4]));
        assert!(source.retained <= crate::source::NATIVE_SOURCE_RETAINED_BYTES);
        assert!(source.state.borrow().metrics.logical_bytes < 1 << 20);
        // Optional export for the installed controlled-HTTP test; sparse holes
        // outside touched pages are intentional and must not be called a full audit.
        if let Ok(target) = std::env::var("SKARVE_LARGE_SKV_FIXTURE") {
            fs::hard_link(&path, target).unwrap();
        }
    }

    #[test]
    fn large_header_capacity_has_fixed_retention_and_checked_group_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = built(dir.path(), false);
        let original = SkvSource::open(&spec(&path), &AtomicBool::new(false)).unwrap();
        let mut header = original.header.clone();
        header.grid.width = 430706;
        header.grid.height = 62971;
        header.chunk_edge = 256;
        header.band_group = 10;
        header.payload_layout = "row_group_v1".into();
        let mut band = header.raw_metadata.bands[0].clone();
        band.scalar_type = crate::source::RawScalarType::Float32;
        header.raw_metadata.bands = (0..36)
            .map(|i| {
                let mut band = band.clone();
                band.original_band_index = i;
                band
            })
            .collect();
        header.raw_metadata.source_band_count = 36;
        header.hierarchy = HierarchyDescription {
            tile_edge: 256,
            levels: SummaryLayout::new(&header.grid, 256, true).unwrap().levels,
        };
        header.validate().unwrap();
        assert_eq!(header.leaves() * header.bands(), 14_904_648);
        assert_eq!(header.data_offset().unwrap(), 2_560_843_584);
        let last = (header.leaves() - 1) * 36;
        assert_eq!(header.physical_neighbors(0).unwrap(), (None, Some(36)));
        assert_eq!(
            header.physical_neighbors(last).unwrap(),
            (Some(last - 36), Some(10))
        );
        assert_eq!(
            header.physical_neighbors(10).unwrap(),
            (Some(last), Some(46))
        );
        assert_eq!(
            header.physical_neighbors(last + 30).unwrap(),
            (Some(last - 6), None)
        );
        assert!(header.physical_neighbors(1).is_err());
        let target = dir.path().join("large-sparse-header.skv");
        let length = 64u64 << 30;
        fs::write(
            &target,
            encode_header(&header, length, &AtomicBool::new(false)).unwrap(),
        )
        .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_len(length)
            .unwrap();
        let opened = SkvSource::open(&spec(&target), &AtomicBool::new(false)).unwrap();
        assert_eq!(opened.state.borrow().intervals.capacity(), MAX_LEAF_RECORDS);
        assert!(opened.retained <= crate::source::NATIVE_SOURCE_RETAINED_BYTES);
        assert_eq!(opened.header.version, 0);
        assert!(opened.record(0, &AtomicBool::new(false)).is_err()); // absent page never accepted
        header.payload_layout = "band".into();
        assert!(header.validate().is_err());
        header.payload_layout = "row_group_v1".into();
        header.grid.width *= 2;
        header.hierarchy.levels = SummaryLayout::new(&header.grid, 256, true).unwrap().levels;
        assert!(header.validate().is_err());
    }

    #[test]
    #[ignore = "bounded preparation roundtrip: run separately from ordinary unit tests"]
    fn large_grouped_complete_verification_streams_without_growing_intervals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-grouped.tif");
        let mut dataset = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<u8, _>(&path, 1, 2049 * 64, 64)
            .unwrap();
        dataset
            .set_geo_transform(&[0., 1., 0., 131136., 0., -1.])
            .unwrap();
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        for band in 1..=64 {
            dataset
                .rasterband(band)
                .unwrap()
                .fill(band as f64, None)
                .unwrap();
        }
        dataset.flush_cache().unwrap();
        drop(dataset);
        let input: SourceSpec = serde_json::from_value(json!({"location":path})).unwrap();
        let cancel = AtomicBool::new(false);
        let source = crate::io::open_source_for_compile(&input, &cancel).unwrap();
        let output = dir.path().join("large-grouped.skv");
        let options = CompileOptions {
            chunk_edge: 64,
            band_group: 64,
            payload_layout: "row_group_v1".into(),
            ..Default::default()
        };
        compile(source.as_ref(), output.to_str().unwrap(), &options, &cancel).unwrap();
        let stored = SkvSource::open(&spec(&output), &cancel).unwrap();
        assert_eq!(stored.header.leaves() * stored.header.bands(), 131136);
        assert!(stored.header.ordered_large_payloads());
        let before = stored.state.borrow().intervals.len();
        verify_complete(&stored, true, &cancel).unwrap();
        assert_eq!(stored.state.borrow().intervals.len(), before);
        assert!(!stored.state.borrow().exhaustive_ordered_verify);
        let (window, _) = stored
            .read_selected_window_cancellable(0, 131130, 1, 6, &[63, 0], 128 << 20, &cancel)
            .unwrap();
        assert_eq!(window.bands[0].values, vec![64.; 6]);
        assert_eq!(window.bands[1].values, vec![1.; 6]);
    }

    #[test]
    fn extended_capacity_compiles_and_round_trips_above_old_record_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tall.tif");
        let height = 65_537 * 64;
        let mut dataset = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<u8, _>(&path, 1, height, 1)
            .unwrap();
        dataset
            .set_geo_transform(&[0., 1., 0., height as f64, 0., -1.])
            .unwrap();
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        dataset
            .rasterband(1)
            .unwrap()
            .write(
                (0, (height - 1) as isize),
                (1, 1),
                &mut Buffer::new((1, 1), vec![197u8]),
            )
            .unwrap();
        dataset.flush_cache().unwrap();
        drop(dataset);
        let cancel = AtomicBool::new(false);
        let input: SourceSpec = serde_json::from_value(json!({"location":path})).unwrap();
        let source = crate::io::open_source_for_compile(&input, &cancel).unwrap();
        let output = dir.path().join("tall.skv");
        let receipt = compile(
            source.as_ref(),
            output.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: 64,
                summaries: false,
                codec: "none".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(receipt["leaf_chunks"], 65_537);
        let opened = SkvSource::open(&spec(&output), &cancel).unwrap();
        let (raw, _) = opened
            .read_raw_selected_window_cancellable(0, height - 1, 1, 1, &[0], 32 << 20, &cancel)
            .unwrap();
        assert_eq!(raw.bands[0].samples_le, vec![197u8]);
        assert_eq!(opened.state.borrow().logical_read_limit, MAX_READS);
        assert!(receipt["verification"]["verified"].as_bool().unwrap());
    }
    #[test]
    fn exhaustive_local_verification_does_not_relax_serving_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = built(dir.path(), false);
        let cancel = AtomicBool::new(false);
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        source.state.borrow_mut().metrics.logical_reads = MAX_READS;
        assert!(
            source
                .state
                .borrow_mut()
                .read("raw", 0, 1, &cancel)
                .is_err()
        );
        verify_complete(&source, true, &cancel).unwrap();
        assert_eq!(source.state.borrow().logical_read_limit, MAX_READS);
        assert!(
            source
                .state
                .borrow_mut()
                .read("raw", 0, 1, &cancel)
                .is_err()
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(verify_complete(&source, true, &cancel).is_err());
        assert_eq!(source.state.borrow().logical_read_limit, MAX_READS);
    }
    #[test]
    fn full_verification_and_parser_reject_coherent_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = built(dir.path(), false);
        let original = fs::read(&path).unwrap();
        let cancel = AtomicBool::new(false);
        let verified = verify(&spec(&path), &cancel).unwrap();
        assert_eq!(verified["leaf_chunks"], 9);
        assert_eq!(verified["summary_states_verified"], 14);
        let bad = dir.path().join("bad.skv");
        for mutation in 0..9 {
            let mut bytes = original.clone();
            match mutation {
                0 => {
                    put_u32(&mut bytes, 8, 1);
                    bootstrap_hash(&mut bytes);
                }
                1 => {
                    bytes.truncate(bytes.len() - 1);
                }
                2 => {
                    bytes[BOOTSTRAP + 32] ^= 1;
                }
                3 => mutate_record(&mut bytes, 0, |r| put_u64(r, 0, u64::MAX - 8)),
                4 => mutate_record(&mut bytes, 0, |r| put_u32(r, 12, u32::MAX)),
                5 => mutate_record(&mut bytes, 0, |r| put_u32(r, 24, 123)),
                6 => mutate_record(&mut bytes, 0, |r| put_u64(r, 80, u64::MAX)),
                7 => mutate_record(&mut bytes, 0, |r| r[127] = 1),
                _ => mutate_record(&mut bytes, 0, |r| put_u64(r, 104, 1)),
            }
            fs::write(&bad, &bytes).unwrap();
            assert!(
                verify(&spec(&bad), &cancel).is_err(),
                "accepted corruption{mutation}"
            );
        }
        let mut bytes = original.clone();
        mutate_record(&mut bytes, 0, |r| {
            let sum = f64::from_bits(u64_at(r, 64));
            put_u64(r, 64, (sum + 1.).to_bits());
        });
        fs::write(&bad, &bytes).unwrap();
        let err = verify(&spec(&bad), &cancel).unwrap_err().to_string();
        assert!(err.contains("does not match raw"), "{err}");
        let first = original[BOOTSTRAP + 16..BOOTSTRAP + 16 + RECORD].to_vec();
        let mut bytes = original.clone();
        mutate_record(&mut bytes, 1, |r| r[..64].copy_from_slice(&first[..64]));
        fs::write(&bad, &bytes).unwrap();
        let err = verify(&spec(&bad), &cancel).unwrap_err().to_string();
        assert!(err.contains("overlapping"), "{err}");
        let mut bytes = original;
        let offset = u64_at(&bytes, BOOTSTRAP + 16) as usize;
        bytes[offset] ^= 1;
        fs::write(&bad, &bytes).unwrap();
        let err = verify(&spec(&bad), &cancel).unwrap_err().to_string();
        assert!(err.contains("payload checksum"), "{err}");
        let opened = SkvSource::open(&spec(&bad), &cancel).unwrap();
        assert!(
            opened
                .read_raw_selected_window_cancellable(0, 0, 2, 2, &[0], 32 << 20, &cancel)
                .is_err()
        );
        assert!(
            opened
                .verify_immutable()
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
    }
    #[test]
    fn codecs_reject_expansion_trailing_data_and_cancellation() {
        let cancel = AtomicBool::new(false);
        let bytes = vec![7u8; 65_537];
        let encoded = encode(&bytes, 1, 3, &cancel).unwrap();
        assert_eq!(decode(&encoded, 1, bytes.len(), &cancel).unwrap(), bytes);
        assert!(decode(&encoded, 1, 10, &cancel).is_err());
        assert!(decode(&encoded, 1, bytes.len() + 1, &cancel).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode(&trailing, 1, bytes.len(), &cancel).is_err());
        assert!(decode(&encoded[..encoded.len() - 2], 1, bytes.len(), &cancel).is_err());
        assert!(decode(&encoded, 2, bytes.len(), &cancel).is_err());
        assert!(decode(&[], 0, usize::MAX, &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert!(decode(&encoded, 1, bytes.len(), &cancel).is_err());
        assert!(encode(&bytes, 1, 3, &cancel).is_err());
    }
    #[test]
    fn raw_survives_unrepresentable_summary_and_resource_failure_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let (path, receipt) = built(dir.path(), true);
        assert_eq!(receipt["summaries"], false);
        assert_eq!(
            receipt["summary_disabled_reason"],
            "nonfinite_or_unrepresentable_compensated_state"
        );
        let cancel = AtomicBool::new(false);
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        assert!(source.stored_summaries().is_none());
        let (raw, _) = source
            .read_raw_selected_window_cancellable(0, 0, 2, 1, &[0], 32 << 20, &cancel)
            .unwrap();
        assert_eq!(
            raw.bands[0].samples_le,
            [f64::MAX.to_le_bytes(), f64::MAX.to_le_bytes()].concat()
        );
        verify(&spec(&path), &cancel).unwrap();
        let target = dir.path().join("limited.skv");
        let options = CompileOptions {
            chunk_edge: 64,
            max_output_bytes: BOOTSTRAP as u64 + PAGE as u64 + 1,
            ..Default::default()
        };
        assert!(compile(&source, target.to_str().unwrap(), &options, &cancel).is_err());
        assert!(!target.exists());
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".incomplete")
        }));
        let low = CompileOptions {
            working_bytes: 16 << 20,
            ..Default::default()
        };
        assert!(compile(&source, target.to_str().unwrap(), &low, &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert!(
            compile(
                &source,
                target.to_str().unwrap(),
                &CompileOptions::default(),
                &cancel
            )
            .is_err()
        );
        assert!(!target.exists());
    }
    struct PositionBoundSource<'a> {
        source: &'a dyn WindowSource,
        bounds: RefCell<Vec<(usize, usize, usize, usize)>>,
        reads: Cell<usize>,
        late_oversize: bool,
        cancel: &'a AtomicBool,
        cancel_at: Option<usize>,
    }
    impl WindowSource for PositionBoundSource<'_> {
        fn metadata(&self) -> &RasterMetadata {
            self.source.metadata()
        }
        fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
            self.source.raw_metadata()
        }
        fn verify_immutable(&self) -> Result<()> {
            self.source.verify_immutable()
        }
        fn raw_read_buffer_bound(&self, _: usize, _: usize, _: &[usize]) -> Result<usize> {
            Ok(512 << 20)
        }
        fn raw_read_buffer_bound_at(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            bands: &[usize],
        ) -> Result<usize> {
            assert_eq!(bands, &[0]);
            let mut bounds = self.bounds.borrow_mut();
            bounds.push((x, y, w, h));
            if self.cancel_at == Some(bounds.len()) {
                self.cancel.store(true, Ordering::Relaxed);
            }
            Ok(if self.late_oversize && x == 128 && y == 128 {
                64 << 20
            } else {
                32 << 20
            })
        }
        fn read_raw_selected_window_cancellable(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            bands: &[usize],
            max_bytes: usize,
            cancel: &AtomicBool,
        ) -> Result<(RawWindow, ReadMetrics)> {
            self.reads.set(self.reads.get() + 1);
            self.source
                .read_raw_selected_window_cancellable(x, y, w, h, bands, max_bytes, cancel)
        }
        fn read_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: usize,
            _: &[usize],
            _: usize,
            _: &AtomicBool,
        ) -> Result<(Raster, ReadMetrics)> {
            anyhow::bail!("unused normalized read")
        }
    }
    #[test]
    fn compiler_admits_every_actual_window_before_output_and_preserves_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let (input, original) = built(dir.path(), false);
        let source_cancel = AtomicBool::new(false);
        let source = SkvSource::open(&spec(&input), &source_cancel).unwrap();
        let options = CompileOptions {
            chunk_edge: 64,
            ..Default::default()
        };
        for (name, late_oversize, cancel_at) in [
            ("admitted", false, None),
            ("late-oversize", true, None),
            ("cancel-before-last", false, Some(8)),
            ("cancel-at-last", false, Some(9)),
        ] {
            let cancel = AtomicBool::new(false);
            let wrapped = PositionBoundSource {
                source: &source,
                bounds: RefCell::new(Vec::new()),
                reads: Cell::new(0),
                late_oversize,
                cancel: &cancel,
                cancel_at,
            };
            let destination = dir.path().join(name);
            fs::create_dir(&destination).unwrap();
            let output = destination.join("result.skv");
            let result = compile(&wrapped, output.to_str().unwrap(), &options, &cancel);
            if name == "admitted" {
                let receipt = result.unwrap();
                assert_eq!(wrapped.reads.get(), 9);
                assert_eq!(receipt["logical_digest"], original["logical_digest"]);
                verify(&spec(&output), &AtomicBool::new(false)).unwrap();
            } else {
                assert!(result.is_err());
                assert_eq!(wrapped.reads.get(), 0);
                assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
                if late_oversize {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("working memory budget")
                    );
                }
            }
            let bounds = wrapped.bounds.borrow();
            assert_eq!(bounds[0], (0, 0, 64, 64));
            assert_eq!(bounds.len(), cancel_at.unwrap_or(9));
            if bounds.len() == 9 {
                assert_eq!(bounds[8], (128, 128, 1, 3));
            }
        }
    }
    struct CancelAfterRead<'a>(&'a dyn WindowSource);
    impl WindowSource for CancelAfterRead<'_> {
        fn metadata(&self) -> &RasterMetadata {
            self.0.metadata()
        }
        fn verify_immutable(&self) -> Result<()> {
            self.0.verify_immutable()
        }
        fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
            self.0.raw_metadata()
        }
        fn raw_read_buffer_bound(&self, w: usize, h: usize, b: &[usize]) -> Result<usize> {
            self.0.raw_read_buffer_bound(w, h, b)
        }
        fn read_raw_selected_window_cancellable(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            b: &[usize],
            m: usize,
            c: &AtomicBool,
        ) -> Result<(RawWindow, ReadMetrics)> {
            let value = self
                .0
                .read_raw_selected_window_cancellable(x, y, w, h, b, m, c)?;
            c.store(true, Ordering::Relaxed);
            Ok(value)
        }
        fn read_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: usize,
            _: &[usize],
            _: usize,
            _: &AtomicBool,
        ) -> Result<(Raster, ReadMetrics)> {
            anyhow::bail!("unused normalized read")
        }
    }
    #[test]
    fn cancelled_partial_build_cleans_only_owned_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = built(dir.path(), false);
        let cancel = AtomicBool::new(false);
        let source = SkvSource::open(&spec(&path), &cancel).unwrap();
        let wrapper = CancelAfterRead(&source);
        let target = dir.path().join("cancel.skv");
        let existing = dir.path().join(".cancel.skv.unrelated.incomplete");
        fs::write(&existing, b"keep unique work").unwrap();
        assert!(
            compile(
                &wrapper,
                target.to_str().unwrap(),
                &CompileOptions::default(),
                &cancel
            )
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(fs::read(&existing).unwrap(), b"keep unique work");
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".incomplete"))
                .count(),
            1
        );
    }
}
