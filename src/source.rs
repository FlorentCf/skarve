//! Native-grid reader contract. Mathematical kernels do not depend on a file
//! format, compression, overviews, tiling, or a prepared value representation.
use crate::model::{Band, Grid, Raster, check_cancel};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Serialize)]
pub struct BandMetadata {
    pub data_type: String,
    pub nodata: Option<f64>,
    pub scale: f64,
    pub offset: f64,
    pub unit: Option<String>,
    /// Physical access hint only; never defines the mathematical grid.
    pub block_size: (usize, usize),
}
#[derive(Clone, Debug, Serialize)]
pub struct RasterMetadata {
    pub grid: Grid,
    pub bands: Vec<BandMetadata>,
    pub source_id: String,
}

/// Original GDAL scalar values admitted by the numerical reader. Raw access
/// preserves their little-endian bits; it never normalizes invalid samples.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawScalarType {
    Byte,
    Int8,
    UInt16,
    Int16,
    UInt32,
    Int32,
    Float32,
    Float64,
}
impl RawScalarType {
    pub fn byte_width(self) -> usize {
        match self {
            Self::Byte | Self::Int8 => 1,
            Self::UInt16 | Self::Int16 => 2,
            Self::UInt32 | Self::Int32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
    pub fn from_gdal_name(name: &str) -> Result<Self> {
        Ok(match name {
            "Byte" => Self::Byte,
            "Int8" => Self::Int8,
            "UInt16" => Self::UInt16,
            "Int16" => Self::Int16,
            "UInt32" => Self::UInt32,
            "Int32" => Self::Int32,
            "Float32" => Self::Float32,
            "Float64" => Self::Float64,
            _ => anyhow::bail!("unsupported raw scalar type {name}"),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawBandMetadata {
    pub scalar_type: RawScalarType,
    /// Exact bits returned by GDAL's f64 NoData metadata API. TIFF tag lexical
    /// spelling and any NaN payload already canonicalized by GDAL are outside
    /// this contract; typed sample payloads are preserved independently.
    pub nodata_f64_bits: Option<u64>,
    pub scale_f64_bits: u64,
    pub offset_f64_bits: u64,
    pub unit: Option<String>,
    /// GDAL GMF_ALL_VALID/PER_DATASET/ALPHA/NODATA bits. The returned mask is
    /// always preserved independently, including nonbinary alpha mask bytes.
    pub mask_flags: u32,
    pub original_band_index: usize,
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawRasterMetadata {
    pub bands: Vec<RawBandMetadata>,
    pub pixel_convention: String,
    pub source_band_count: usize,
    /// Original explicitly selected TIFF overview (zero-based GDAL index).
    /// The exposed grid is this existing view; no query-time resampling occurs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_overview: Option<usize>,
}
#[derive(Debug)]
pub struct RawBandWindow {
    pub samples_le: Vec<u8>,
    pub mask: Vec<u8>,
}
#[derive(Debug)]
pub struct RawWindow {
    pub width: usize,
    pub height: usize,
    pub bands: Vec<RawBandWindow>,
}
/// Shared interpretation used by both GDAL normalized reads and typed SKV
/// decoding. Keep the finite/raw-mask/NoData predicate before scale and offset.
pub(crate) fn normalize_raw_sample(
    raw: f64,
    mask: u8,
    nodata: Option<f64>,
    scale: f64,
    offset: f64,
) -> (f64, bool) {
    let present = mask != 0
        && raw.is_finite()
        && !nodata.is_some_and(|missing| raw == missing || (raw.is_nan() && missing.is_nan()));
    let decoded = raw * scale + offset;
    let valid = present && decoded.is_finite();
    (if valid { decoded } else { 0.0 }, valid)
}
/// The existing32KiB reserve covers grid/identity and the first20 band records.
/// Wider source windows additionally reserve each cloned unit string and band
/// record; this does not change any reader or query byte limit.
pub(crate) fn source_window_overhead(bands: usize) -> usize {
    32 * 1024 + bands.saturating_sub(20) * 2048
}
impl RawWindow {
    /// Normalize a bounded selected-band group under the ordinary source policy.
    /// `max_bytes` covers this method's new f64/validity output, not the borrowed
    /// raw input, which remains charged to its owner's separate read reservation.
    #[allow(clippy::too_many_arguments)]
    pub fn normalize(
        &self,
        raw_metadata: &RawRasterMetadata,
        metadata: &RasterMetadata,
        x: usize,
        y: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<Raster> {
        check_cancel(cancel)?;
        ensure!(
            self.width > 0
                && self.height > 0
                && x.checked_add(self.width)
                    .is_some_and(|n| n <= metadata.grid.width)
                && y.checked_add(self.height)
                    .is_some_and(|n| n <= metadata.grid.height),
            "typed normalization window lies outside raster"
        );
        ensure!(
            !indices.is_empty()
                && indices.len() <= 64
                && indices.len() == self.bands.len()
                && indices
                    .iter()
                    .all(|&i| i < raw_metadata.bands.len() && i < metadata.bands.len()),
            "typed normalization requires 1..64 matching source-window bands"
        );
        let cells = self
            .width
            .checked_mul(self.height)
            .ok_or_else(|| anyhow::anyhow!("typed normalization size overflow"))?;
        let needed = cells
            .checked_mul(indices.len())
            .and_then(|n| n.checked_mul(9))
            .and_then(|n| n.checked_add(source_window_overhead(indices.len())))
            .ok_or_else(|| anyhow::anyhow!("typed normalization buffer overflow"))?;
        ensure!(
            needed <= max_bytes,
            "typed normalization exceeds output memory budget"
        );
        let mut bands = Vec::with_capacity(indices.len());
        for (&index, input) in indices.iter().zip(&self.bands) {
            let info = &raw_metadata.bands[index];
            let width = info.scalar_type.byte_width();
            ensure!(
                input.mask.len() == cells
                    && input.samples_le.len()
                        == cells
                            .checked_mul(width)
                            .ok_or_else(|| anyhow::anyhow!("typed normalization byte overflow"))?,
                "typed sample or mask length mismatch"
            );
            let scale = f64::from_bits(info.scale_f64_bits);
            let offset = f64::from_bits(info.offset_f64_bits);
            ensure!(
                scale.is_finite() && offset.is_finite(),
                "nonfinite typed scale or offset"
            );
            let nodata = info.nodata_f64_bits.map(f64::from_bits);
            let mut output = Band {
                values: vec![0.; cells],
                valid: vec![false; cells],
                unit: info.unit.clone(),
            };
            for (cell, (bytes, &mask)) in input
                .samples_le
                .chunks_exact(width)
                .zip(&input.mask)
                .enumerate()
            {
                if cell % 4096 == 0 {
                    check_cancel(cancel)?;
                }
                macro_rules! scalar {
                    ($kind:ty) => {
                        <$kind>::from_le_bytes(bytes.try_into().expect("validated scalar width"))
                            as f64
                    };
                }
                let raw = match info.scalar_type {
                    RawScalarType::Byte => scalar!(u8),
                    RawScalarType::Int8 => scalar!(i8),
                    RawScalarType::UInt16 => scalar!(u16),
                    RawScalarType::Int16 => scalar!(i16),
                    RawScalarType::UInt32 => scalar!(u32),
                    RawScalarType::Int32 => scalar!(i32),
                    RawScalarType::Float32 => scalar!(f32),
                    RawScalarType::Float64 => scalar!(f64),
                };
                (output.values[cell], output.valid[cell]) =
                    normalize_raw_sample(raw, mask, nodata, scale, offset);
            }
            bands.push(output);
        }
        let mut grid = metadata.grid.clone();
        grid.width = self.width;
        grid.height = self.height;
        grid.transform[0] += x as f64 * grid.transform[1];
        grid.transform[3] += y as f64 * grid.transform[5];
        check_cancel(cancel)?;
        Ok(Raster {
            grid,
            bands,
            source_id: metadata.source_id.clone(),
        })
    }
}
#[derive(Default, Serialize)]
pub struct ReadMetrics {
    pub read_decode_ms: f64,
    pub normalization_ms: f64,
    /// Adapter calls, not a claim about physical disk/network reads.
    pub raster_io_calls: usize,
    /// Successful mapped-read footprint transitions plus final cache release.
    pub decoder_cache_flushes: usize,
    pub decoder_cache_reused_chunks: usize,
}

/// A stable declared source grid with independently normalized band masks.
/// Return bands in requested order, never resample, and reject changes/cancel.
/// `max_bytes` bounds returned values/masks; adapter caches must be separately
/// bounded and reported. Window coordinates address the declared grid, including
/// an explicitly selected existing stored view; the reader never resamples.
pub trait WindowSource {
    fn metadata(&self) -> &RasterMetadata;
    fn verify_immutable(&self) -> Result<()>;
    /// Maximum bands in one transient raw or normalized source read. Ordinary
    /// readers retain20. An implementation may admit up to64 only when its
    /// reported read-buffer bound covers the complete returned window.
    fn max_read_bands(&self) -> usize {
        20
    }
    /// Optional original typed access for lossless derived-format compilation.
    /// Absence is explicit: normalized values cannot reconstruct invalid bits.
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        None
    }
    fn raw_read_buffer_bound(
        &self,
        _width: usize,
        _height: usize,
        _indices: &[usize],
    ) -> Result<usize> {
        anyhow::bail!("source does not support lossless typed reads")
    }
    #[allow(clippy::too_many_arguments)]
    fn read_raw_selected_window_cancellable(
        &self,
        _x: usize,
        _y: usize,
        _width: usize,
        _height: usize,
        _indices: &[usize],
        _max_bytes: usize,
        _cancel: &AtomicBool,
    ) -> Result<(RawWindow, ReadMetrics)> {
        anyhow::bail!("source does not support lossless typed reads")
    }
    /// Optional stored numerical states; eligibility is determined by the
    /// existing kernels and query policy, never by the file suffix.
    fn stored_summaries(&self) -> Option<&dyn crate::stored_summary::StoredSummarySource> {
        None
    }
    /// Sanitized cumulative access diagnostics; never includes locations or auth.
    fn diagnostics(&self) -> Value {
        serde_json::json!({})
    }
    /// Portable analytical identity, when explicitly established by the adapter.
    fn identity_descriptor(&self) -> Option<Value> {
        None
    }
    /// Physical hints are independent of mathematical grid identity.
    fn access_layout(&self) -> Value {
        serde_json::json!({})
    }
    /// Maximum retained adapter/cache capacity, including future cache growth.
    fn retained_memory_bound(&self) -> usize {
        0
    }
    /// Output plus per-read adapter buffers. Persistent execution reserves this
    /// in addition to its geometry, summary and destination buffers.
    fn read_buffer_bound(&self, width: usize, height: usize, indices: &[usize]) -> Result<usize> {
        width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(indices.len()))
            .and_then(|n| n.checked_mul(9))
            .ok_or_else(|| anyhow::anyhow!("source window allocation overflow"))
    }
    #[allow(clippy::too_many_arguments)]
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)>;
    /// Opt-in to bounded two-leaf preparation in the stored-summary consumer.
    /// Other consumers and arbitrary readers retain their existing eager reads.
    fn boundary_prefetch_enabled(&self) -> bool {
        false
    }
    /// Prepare only the admitted positive boundary windows and exact band set.
    /// `max_bytes` is scratch remaining after queued coverage capacity; retained
    /// cache allocations must already be included in retained_memory_bound().
    /// Implementations preserve range/identity/cancellation limits and may decline
    /// preparation without reading. This does not return or retain decoded output.
    fn prefetch_boundary_windows(
        &self,
        _windows: &[[usize; 4]],
        _bands: &[usize],
        _max_bytes: usize,
        _cancel: &AtomicBool,
    ) -> Result<()> {
        Ok(())
    }
}

/// Physical-window policy receipt. Incidents count native data-block/window
/// intersections, not codec calls or physical byte transfers.
#[derive(Clone, Debug, Serialize)]
pub struct WindowPolicyCandidate {
    pub edge: usize,
    pub windows: usize,
    pub decoded_cells: u64,
    pub data_block_incidents: u64,
    pub read_buffer_bound_bytes: usize,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct WindowPolicyDecision {
    pub requested_edge: usize,
    pub selected_edge: usize,
    pub planner_capacity_bound_bytes: usize,
    pub candidates: Vec<WindowPolicyCandidate>,
}
/// Select a direct-execution I/O edge from actual occupied base tiles (ty, tx).
/// This never changes the native grid, coverage weights or prepared index edge.
/// At most 16,384 input tiles and five candidate edges are examined. Promotion
/// requires a selected square physical-block layout, <=2x logical decode cells,
/// fewer windows, no increase in data-block incidents, and a fitting read bound.
/// Caller owns geometry recompilation and any expression/reducer scratch bound.
pub fn select_window_edge(
    source: &dyn WindowSource,
    requested_edge: usize,
    indices: &[usize],
    occupied_tiles: &[(usize, usize)],
    max_bytes: usize,
) -> Result<WindowPolicyDecision> {
    use std::collections::BTreeSet;
    ensure!(
        [32, 64, 128, 256, 512].contains(&requested_edge),
        "unsupported source-window base edge"
    );
    ensure!(
        occupied_tiles.len() <= 16384
            && !indices.is_empty()
            && indices.len() <= source.max_read_bands().min(64),
        "source-window policy input exceeds budget"
    );
    let meta = source.metadata();
    let grid = &meta.grid;
    let mut physical_edge = requested_edge;
    let mut square_layout = true;
    for &i in indices {
        let band = meta
            .bands
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("source-window band out of range"))?;
        let (w, h) = band.block_size;
        ensure!(w > 0 && h > 0, "invalid source-window block dimensions");
        square_layout &= w == h && w.is_power_of_two();
        physical_edge = physical_edge.max(w.min(512));
    }
    for &(ty, tx) in occupied_tiles {
        ensure!(
            tx < grid.width.div_ceil(requested_edge) && ty < grid.height.div_ceil(requested_edge),
            "source-window tile out of bounds"
        );
    }
    let mut decision = WindowPolicyDecision {
        requested_edge,
        selected_edge: requested_edge,
        // Conservatively covers input-copy/tree nodes plus candidate receipts;
        // source.read_buffer_bound does not allocate or read native values.
        planner_capacity_bound_bytes: occupied_tiles
            .len()
            .saturating_mul(192)
            .saturating_add(4096),
        candidates: Vec::new(),
    };
    for edge in [32, 64, 128, 256, 512]
        .into_iter()
        .filter(|&edge| edge >= requested_edge)
    {
        if edge != requested_edge && (!square_layout || edge > physical_edge) {
            continue;
        }
        let tiles = occupied_tiles
            .iter()
            .map(|&(ty, tx)| (ty * requested_edge / edge, tx * requested_edge / edge))
            .collect::<BTreeSet<_>>();
        let mut cells = 0u64;
        let mut incidents = 0u64;
        let mut bound = 0usize;
        let mut bound_refused = false;
        for &(ty, tx) in &tiles {
            let (x, y) = (tx * edge, ty * edge);
            let (w, h) = (edge.min(grid.width - x), edge.min(grid.height - y));
            cells = cells
                .checked_add((w * h) as u64)
                .ok_or_else(|| anyhow::anyhow!("window cell estimate overflow"))?;
            match source.read_buffer_bound(w, h, indices) {
                Ok(bytes) => bound = bound.max(bytes),
                Err(error) if edge == requested_edge => return Err(error),
                Err(_) => {
                    bound_refused = true;
                    bound = usize::MAX;
                }
            }
            for &i in indices {
                let (bw, bh) = meta.bands[i].block_size;
                let count =
                    ((x + w - 1) / bw - x / bw + 1) as u64 * ((y + h - 1) / bh - y / bh + 1) as u64;
                incidents = incidents
                    .checked_add(count)
                    .ok_or_else(|| anyhow::anyhow!("physical incident estimate overflow"))?;
            }
        }
        let (eligible, reason) = if edge == requested_edge {
            (bound <= max_bytes, "requested_baseline")
        } else {
            let base = &decision.candidates[0];
            if bound_refused {
                (false, "optional_read_bound_refused")
            } else if bound > max_bytes {
                (false, "read_buffer_budget")
            } else if cells > base.decoded_cells.saturating_mul(2) {
                (false, "sparse_logical_overread")
            } else if incidents > base.data_block_incidents {
                (false, "physical_block_incident_increase")
            } else if tiles.len() >= base.windows {
                (false, "no_window_reduction")
            } else {
                (true, "bounded_physical_coalescing")
            }
        };
        if eligible {
            decision.selected_edge = edge;
        }
        decision.candidates.push(WindowPolicyCandidate {
            edge,
            windows: tiles.len(),
            decoded_cells: cells,
            data_block_incidents: incidents,
            read_buffer_bound_bytes: bound,
            eligible,
            reason: reason.into(),
        });
    }
    Ok(decision)
}

/// Conservative retained capacity of the built-in original-source adapter.
/// Callers reserve this before opening; window decode buffers are additional.
pub const NATIVE_SOURCE_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// Explicit trusted SDK input. Applications resolve their own source allowlist.
/// Intentionally not Serialize/Debug: locations and headers may contain secrets.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub location: String,
    #[serde(default)]
    pub format: SourceFormat,
    #[serde(default)]
    pub variable: Option<String>,
    /// Explicit existing TIFF overview, zero-based (0 is the first reduced
    /// image). Omission always means native resolution. Never selected from
    /// query size, and never synthesized by resampling.
    #[serde(default)]
    pub overview: Option<usize>,
    /// Explicit interpretation for a source lacking CRS. Existing conflicting
    /// CRS metadata is rejected; this never reprojects coordinates or pixels.
    #[serde(default)]
    pub crs: Option<String>,
    /// Explicit whole-axis longitude representation translation for NetCDF.
    /// Only -360,0,+360 degrees; no rotation/reordering or antimeridian repair.
    #[serde(default)]
    pub longitude_shift: i32,
    /// Zero-based native band mapping, in exposed order.
    #[serde(default)]
    pub bands: Option<Vec<usize>>,
    /// Disable optional embedded summaries while retaining the same source data
    /// and interpretation. This is an execution ablation, not a new dataset.
    #[serde(default = "source_summaries_enabled")]
    pub use_summaries: bool,
    #[serde(default)]
    pub identity: Option<ContentIdentity>,
    #[serde(default)]
    pub http: HttpOptions,
}
fn source_summaries_enabled() -> bool {
    true
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    #[default]
    Geotiff,
    Netcdf,
    Skv,
}

/// Shared source admission, including callers that open SKV directly for full
/// verification instead of registering an ordinary source handle first.
pub(crate) fn validate_source_spec(spec: &SourceSpec) -> Result<()> {
    ensure!(
        !spec.location.is_empty() && spec.location.len() <= 16384,
        "source location exceeds budget"
    );
    ensure!(
        !spec.location.contains(['\0', '\n', '\r']),
        "invalid source location"
    );
    let remote = spec.location.starts_with("https://") || spec.location.starts_with("http://");
    ensure!(
        spec.overview
            .is_none_or(|level| level < 64 && spec.format == SourceFormat::Geotiff),
        "overview requires a GeoTIFF source and a zero-based index below64"
    );
    ensure!(
        remote || (!spec.location.contains("://") && !spec.location.starts_with("/vsi")),
        "unsupported source transport"
    );
    ensure!(
        [-360, 0, 360].contains(&spec.longitude_shift)
            && (spec.longitude_shift == 0 || spec.format == SourceFormat::Netcdf),
        "longitude shift requires NetCDF and0 or plus/minus360 degrees"
    );
    ensure!(
        spec.http.max_requests > 0
            && spec.http.max_requests <= 65536
            && spec.http.max_download_bytes > 0
            && spec.http.max_download_bytes <= 768 * 1024 * 1024
            && spec.http.max_range_bytes > 0
            && spec.http.max_range_bytes <= 4 * 1024 * 1024
            && spec.http.timeout_seconds > 0
            && spec.http.timeout_seconds <= 30
            && spec.http.cache_bytes <= 8 * 1024 * 1024,
        "source transport limits exceed supported budget"
    );
    if let Some(ids) = &spec.bands {
        ensure!(
            !ids.is_empty()
                && ids.len() <= 64
                && ids
                    .iter()
                    .enumerate()
                    .all(|(i, band)| !ids[..i].contains(band)),
            "source band mapping must contain1..64 distinct indices"
        );
    }
    if let Some(identity) = &spec.identity {
        ensure!(
            identity.sha256.len() == 64
                && identity.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                && identity.byte_length > 0,
            "invalid content identity"
        );
    }
    if let Some(crs) = &spec.crs {
        ensure!(
            !crs.is_empty() && crs.len() <= 4096,
            "explicit source CRS exceeds budget"
        );
    }
    Ok(())
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    Verify,
    TrustedManifest,
}
#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContentIdentity {
    pub sha256: String,
    pub byte_length: u64,
    pub policy: VerificationPolicy,
    /// Object-scoped expected strong ETag for a trusted remote manifest.
    #[serde(default)]
    pub etag: Option<String>,
}
#[derive(Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpOptions {
    pub headers: BTreeMap<String, String>,
    /// HTTP header -> environment variable name. Resolved only when opening.
    pub header_env: BTreeMap<String, String>,
    pub max_requests: u64,
    pub max_download_bytes: u64,
    pub max_range_bytes: u64,
    pub timeout_seconds: u64,
    /// Explicit opt-in for controlled local HTTP fixtures; HTTPS is ordinary.
    pub allow_http: bool,
    pub cache_bytes: usize,
    /// Enable eligible SKV summary-HTTP boundary preparation; false is an eager ablation.
    pub boundary_read_ahead: bool,
}
impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            headers: BTreeMap::new(),
            header_env: BTreeMap::new(),
            max_requests: 4096,
            max_download_bytes: 128 * 1024 * 1024,
            max_range_bytes: 4 * 1024 * 1024,
            timeout_seconds: 30,
            allow_http: false,
            cache_bytes: 4 * 1024 * 1024,
            boundary_read_ahead: true,
        }
    }
}

/// Enforce the reader contract before caching, reducing, or persisting a window.
/// A valid raster alone does not prove its position or requested band ordering.
pub(crate) fn validate_window(
    metadata: &RasterMetadata,
    raster: &Raster,
    window: [usize; 4],
    indices: &[usize],
) -> Result<()> {
    validate_window_with(
        metadata,
        raster,
        window,
        indices,
        Raster::validate_source_window,
    )
}

/// Structural postcondition only for SKV values made finite/invalid by its
/// normalizer. Never use this in place of full validation at consumer boundaries.
/// It still validates the shifted grid, allocation bounds, identity and bands.
pub(crate) fn validate_window_structure(
    metadata: &RasterMetadata,
    raster: &Raster,
    window: [usize; 4],
    indices: &[usize],
) -> Result<()> {
    validate_window_with(
        metadata,
        raster,
        window,
        indices,
        Raster::validate_source_window_structure,
    )
}

fn validate_window_with(
    metadata: &RasterMetadata,
    raster: &Raster,
    window: [usize; 4],
    indices: &[usize],
    validate_raster: impl FnOnce(&Raster) -> Result<()>,
) -> Result<()> {
    let [x, y, width, height] = window;
    ensure!(
        x.checked_add(width)
            .is_some_and(|end| end <= metadata.grid.width)
            && y.checked_add(height)
                .is_some_and(|end| end <= metadata.grid.height),
        "source returned out-of-bounds native window"
    );
    let mut expected_grid = metadata.grid.clone();
    expected_grid.width = width;
    expected_grid.height = height;
    expected_grid.transform[0] += x as f64 * metadata.grid.transform[1];
    expected_grid.transform[3] += y as f64 * metadata.grid.transform[5];
    ensure!(
        raster.grid == expected_grid,
        "source changed native window alignment"
    );
    ensure!(
        raster.source_id == metadata.source_id && raster.bands.len() == indices.len(),
        "source returned incompatible window identity or bands"
    );
    validate_raster(raster)?;
    for (band, &index) in raster.bands.iter().zip(indices) {
        ensure!(
            metadata
                .bands
                .get(index)
                .is_some_and(|expected| band.unit == expected.unit),
            "source returned incompatible band unit"
        );
    }
    Ok(())
}

#[cfg(test)]
mod window_structure_tests {
    use super::*;
    use crate::model::Band;

    fn fixture() -> (RasterMetadata, Raster) {
        let grid = Grid {
            width: 1,
            height: 1,
            transform: [0., 1., 0., 1., 0., -1.],
            crs: "LOCAL".into(),
        };
        (
            RasterMetadata {
                grid: grid.clone(),
                bands: vec![BandMetadata {
                    data_type: "Float64".into(),
                    nodata: None,
                    scale: 1.,
                    offset: 0.,
                    unit: Some("people".into()),
                    block_size: (64, 64),
                }],
                source_id: "structure-fixture".into(),
            },
            Raster {
                grid,
                bands: vec![Band {
                    values: vec![1.],
                    valid: vec![true],
                    unit: Some("people".into()),
                }],
                source_id: "structure-fixture".into(),
            },
        )
    }

    #[test]
    fn shifted_grid_precision_remains_a_structural_error() {
        let (mut metadata, mut raster) = fixture();
        metadata.grid.width = 2;
        metadata.grid.transform[0] = ((1u64 << 53) - 1) as f64;
        metadata.grid.validate().unwrap();
        raster.grid.transform[0] = metadata.grid.transform[0] + 1.;
        let error = validate_window_structure(&metadata, &raster, [1, 0, 1, 1], &[0]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("precision cannot resolve a cell")
        );
        assert!(validate_window(&metadata, &raster, [1, 0, 1, 1], &[0]).is_err());
    }

    #[test]
    fn finite_parent_grid_does_not_admit_an_overflowed_shift() {
        let (mut metadata, mut raster) = fixture();
        metadata.grid.width = 3;
        metadata.grid.transform[0] = f64::MAX / 2.;
        metadata.grid.transform[1] = f64::MAX / 2.;
        metadata.grid.validate().unwrap();
        raster.grid.transform[0] = metadata.grid.transform[0] + 2. * metadata.grid.transform[1];
        raster.grid.transform[1] = metadata.grid.transform[1];
        assert!(raster.grid.transform[0].is_infinite());
        assert!(
            validate_window_structure(&metadata, &raster, [2, 0, 1, 1], &[0])
                .unwrap_err()
                .to_string()
                .contains("nonfinite affine")
        );
    }

    #[test]
    fn structure_keeps_unit_metadata_identity_and_shape_guards() {
        for fault in ["unit_cap", "unit_mapping", "identity", "mask", "crs_cap"] {
            let (mut metadata, mut raster) = fixture();
            match fault {
                "unit_cap" => {
                    metadata.bands[0].unit = Some("u".repeat(1025));
                    raster.bands[0].unit = metadata.bands[0].unit.clone();
                }
                "unit_mapping" => raster.bands[0].unit = Some("wrong".into()),
                "identity" => raster.source_id.push_str("-changed"),
                "mask" => raster.bands[0].valid.clear(),
                "crs_cap" => {
                    metadata.grid.crs = "c".repeat(4097);
                    raster.grid.crs = metadata.grid.crs.clone();
                }
                _ => unreachable!(),
            }
            assert!(
                validate_window_structure(&metadata, &raster, [0, 0, 1, 1], &[0]).is_err(),
                "{fault}"
            );
            assert!(
                validate_window(&metadata, &raster, [0, 0, 1, 1], &[0]).is_err(),
                "{fault}"
            );
        }
    }

    #[test]
    fn full_consumer_contract_still_rejects_valid_nonfinite_samples() {
        let (metadata, mut raster) = fixture();
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            raster.bands[0].values[0] = value;
            raster.bands[0].valid[0] = true;
            validate_window_structure(&metadata, &raster, [0, 0, 1, 1], &[0]).unwrap();
            assert!(
                validate_window(&metadata, &raster, [0, 0, 1, 1], &[0])
                    .unwrap_err()
                    .to_string()
                    .contains("valid values must be finite")
            );
            raster.bands[0].valid[0] = false;
            validate_window(&metadata, &raster, [0, 0, 1, 1], &[0]).unwrap();
        }
    }
}
