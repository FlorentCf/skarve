//! Bounded, versioned, independently checksummed summaries with optional raw pages.
//! A complete index is committed atomically. No query scans the raw source
//! to verify identity, and no fully covered eligible tile requires a raw page.
use crate::{
    aggregate::{Acc, Options},
    hierarchy::{GeometryPredicate, Relation},
    io::{LocalSource, RangeSource, RemoteLimits},
    model::*,
    source::WindowSource,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const HEADER: usize = 65536;
const MAGIC: &[u8; 8] = b"RSTRLAB1";
const MAX_PAIR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_QUERY_BYTES: u64 = 128 * 1024 * 1024;
const RANGE: usize = 4 * 1024 * 1024;
const CAPABILITIES: [&str; 5] = ["sum", "support", "count", "min", "max"];

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub version: u32,
    pub kind: String,
    pub numerical_contract: String,
    pub algorithm_version: String,
    pub grid: Grid,
    pub source_id: String,
    pub source_metadata: Value,
    pub build_id: String,
    pub units: Vec<Option<String>>,
    pub tile_edge: usize,
    pub band_group: usize,
    pub capabilities: Vec<String>,
    pub byte_order: String,
    pub scalar: String,
    #[serde(default)]
    pub boundary_source: BoundarySource,
    /// v2 portable content/interpretation identity; absent from legacy v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<Value>,
}
/// Missing field in earlier v1 files means the original normalized pair.
#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundarySource {
    #[default]
    Normalized,
    Original,
}
impl Header {
    pub fn tiles_x(&self) -> usize {
        self.grid.width.div_ceil(self.tile_edge)
    }
    pub fn tiles_y(&self) -> usize {
        self.grid.height.div_ceil(self.tile_edge)
    }
    pub fn tiles(&self) -> usize {
        self.tiles_x() * self.tiles_y()
    }
    fn groups(&self) -> usize {
        self.units.len().div_ceil(self.band_group)
    }
    fn summary_size(&self) -> usize {
        self.units.len() * 40 + 32
    }
    pub fn levels(&self) -> Vec<(usize, usize, usize)> {
        let (mut x, mut y, mut offset) = (self.tiles_x(), self.tiles_y(), 0);
        let mut levels = vec![];
        loop {
            levels.push((x, y, offset));
            if self.algorithm_version == "flat_tiles_v1" || (x == 1 && y == 1) {
                break;
            }
            offset += x * y;
            x = x.div_ceil(2);
            y = y.div_ceil(2);
        }
        levels
    }
    pub fn summary_records(&self) -> usize {
        self.levels().iter().map(|(x, y, _)| x * y).sum()
    }
    fn raw_size(&self) -> usize {
        self.tile_edge * self.tile_edge * self.band_group * 9 + 32
    }
    fn file_length(&self) -> u64 {
        HEADER as u64
            + if self.kind == "summaries" {
                self.summary_records() as u64 * self.summary_size() as u64
            } else {
                self.tiles() as u64 * self.groups() as u64 * self.raw_size() as u64
            }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            [1, 2].contains(&self.version) && ["summaries", "raw"].contains(&self.kind.as_str()),
            "unsupported persistent format version/kind"
        );
        ensure!(
            (self.version == 1 && self.source_identity.is_none())
                || (self.version == 2
                    && self.source_identity.as_ref().is_some_and(|v| v.is_object())),
            "persistent identity/version mismatch"
        );
        ensure!(
            self.numerical_contract == "native_grid_planar_v1"
                && ["flat_tiles_v1", "hierarchy_tiles_v1"]
                    .contains(&self.algorithm_version.as_str()),
            "unsupported persistent numerical/algorithm version"
        );
        self.grid.validate()?;
        ensure!(
            (1..=20).contains(&self.units.len())
                && self
                    .units
                    .iter()
                    .all(|u| u.as_ref().is_none_or(|s| s.len() <= 1024)),
            "invalid persistent band metadata"
        );
        ensure!(
            [16, 64, 256].contains(&self.tile_edge)
                && (1..=20).contains(&self.band_group)
                && self.band_group <= self.units.len(),
            "invalid persistent tile/group dimensions"
        );
        ensure!(
            self.source_id.len() <= 1024
                && self.build_id.len() == 64
                && self.build_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid persistent identity"
        );
        ensure!(
            self.byte_order == "little"
                && self.scalar == "normalized_f64_u8_valid"
                && self.capabilities == CAPABILITIES,
            "unsupported persistent layout/capabilities"
        );
        ensure!(
            self.file_length() <= MAX_PAIR_BYTES,
            "persistent file exceeds format budget"
        );
        Ok(())
    }
    pub fn bounds(&self, tile: usize) -> [usize; 4] {
        let x = tile % self.tiles_x() * self.tile_edge;
        let y = tile / self.tiles_x() * self.tile_edge;
        [
            x,
            y,
            (x + self.tile_edge).min(self.grid.width),
            (y + self.tile_edge).min(self.grid.height),
        ]
    }
}
fn checksum_page(mut data: Vec<u8>) -> Vec<u8> {
    let hash = blake3::hash(&data);
    data.extend_from_slice(hash.as_bytes());
    data
}
fn verify_page(data: &[u8]) -> Result<&[u8]> {
    ensure!(data.len() >= 32, "truncated persistent page");
    let body = &data[..data.len() - 32];
    ensure!(
        blake3::hash(body).as_bytes() == &data[data.len() - 32..],
        "persistent page checksum mismatch"
    );
    Ok(body)
}
fn encode_header(h: &Header) -> Result<Vec<u8>> {
    h.validate()?;
    let json = serde_json::to_vec(h)?;
    ensure!(
        json.len() <= HEADER - 48,
        "persistent header exceeds budget"
    );
    let mut bytes = vec![0; HEADER - 32];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..16].copy_from_slice(&(json.len() as u64).to_le_bytes());
    bytes[16..16 + json.len()].copy_from_slice(&json);
    Ok(checksum_page(bytes))
}
fn decode_header(bytes: &[u8]) -> Result<Header> {
    let b = verify_page(bytes)?;
    ensure!(
        b.len() == HEADER - 32 && &b[..8] == MAGIC,
        "incomplete or invalid persistent header"
    );
    let n = u64::from_le_bytes(b[8..16].try_into()?) as usize;
    ensure!(
        n <= HEADER - 48,
        "persistent header allocation bound exceeded"
    );
    ensure!(
        b[16 + n..].iter().all(|&v| v == 0),
        "noncanonical persistent header padding"
    );
    let h: Header = serde_json::from_slice(&b[16..16 + n])?;
    h.validate()?;
    Ok(h)
}
fn append_f64(out: &mut Vec<u8>, value: f64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn read_f64(data: &[u8], pos: usize) -> f64 {
    f64::from_le_bytes(
        data[pos..pos + 8]
            .try_into()
            .expect("validated page length"),
    )
}

/// Preparation is optional. Original mode keeps only summaries; normalized mode
/// explicitly duplicates native-resolution values and reports all duplicate bytes.
pub fn build(
    path: &str,
    destination: &str,
    tile_edge: usize,
    layout: &str,
    summary_backend: &str,
    boundary_source: BoundarySource,
    cancel: &AtomicBool,
) -> Result<Value> {
    let source = LocalSource::open(path)?;
    build_from_source(
        &source,
        destination,
        tile_edge,
        layout,
        summary_backend,
        boundary_source,
        cancel,
    )
}

/// Preparation consumes the same native-window contract as direct execution.
/// The source remains an explicit dependency when boundary_source is Original.
pub fn build_from_source(
    source: &dyn WindowSource,
    destination: &str,
    tile_edge: usize,
    layout: &str,
    summary_backend: &str,
    boundary_source: BoundarySource,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    check_cancel(cancel)?;
    source.verify_immutable()?;
    ensure!(
        ["flat", "hierarchy"].contains(&summary_backend),
        "summary_backend must be flat or hierarchy"
    );
    ensure!(
        [16, 64, 256].contains(&tile_edge),
        "tile_edge must be 16, 64 or 256"
    );
    let n = source.metadata().bands.len();
    source.metadata().grid.validate()?;
    ensure!((1..=20).contains(&n), "source requires 1..20 bands");
    let group = match layout {
        "band_major" => 1,
        "cell_major" => n,
        "band_groups_4" => 4.min(n),
        _ => anyhow::bail!("unknown prepared layout"),
    };
    let destination = Path::new(destination);
    ensure!(
        !destination.exists(),
        "index destination already exists; choose a new version path"
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(parent.is_dir(), "index destination parent must exist");
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let build_id = blake3::hash(
        format!(
            "{}:{nonce}:{}",
            source.metadata().source_id,
            std::process::id()
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string();
    let mut header = Header {
        version: if source.identity_descriptor().is_some() {
            2
        } else {
            1
        },
        kind: "summaries".into(),
        numerical_contract: "native_grid_planar_v1".into(),
        algorithm_version: if summary_backend == "hierarchy" {
            "hierarchy_tiles_v1"
        } else {
            "flat_tiles_v1"
        }
        .into(),
        grid: source.metadata().grid.clone(),
        source_id: source.metadata().source_id.clone(),
        source_metadata: serde_json::to_value(source.metadata())?,
        build_id: build_id.clone(),
        units: source
            .metadata()
            .bands
            .iter()
            .map(|b| b.unit.clone())
            .collect(),
        tile_edge,
        band_group: group,
        capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        byte_order: "little".into(),
        scalar: "normalized_f64_u8_valid".into(),
        boundary_source,
        source_identity: source.identity_descriptor(),
    };
    header.validate()?;
    let index_bytes = header.file_length();
    header.kind = "raw".into();
    let raw_bytes = if boundary_source == BoundarySource::Normalized {
        header.file_length()
    } else {
        0
    };
    header.kind = "summaries".into();
    ensure!(
        index_bytes + raw_bytes <= MAX_PAIR_BYTES,
        "persistent pair exceeds 8 GiB build output budget"
    );
    let builder_extra = tile_edge * tile_edge * group * 9 + header.summary_size() + 2 * HEADER;
    let working_bytes = source
        .read_buffer_bound(tile_edge, tile_edge, &(0..n).collect::<Vec<_>>())?
        .checked_add(builder_extra)
        .context("builder memory bound overflow")?;
    ensure!(
        working_bytes <= 256 * 1024 * 1024,
        "index builder working memory budget exceeded"
    );
    // Striped sources already decode whole native rows. Read each row group
    // once when it fits, while preserving the existing per-tile reduction order.
    // A physical hint only changes access: values, summaries and the format do
    // not depend on it. Failure to admit this optional buffer keeps tile reads.
    let stripe_bound = if source
        .metadata()
        .bands
        .iter()
        .all(|b| b.block_size.0 == header.grid.width && b.block_size.1 <= tile_edge)
        && header.grid.width > tile_edge
    {
        source
            .read_buffer_bound(
                header.grid.width,
                tile_edge.min(header.grid.height),
                &(0..n).collect::<Vec<_>>(),
            )
            .ok()
            .and_then(|b| b.checked_add(builder_extra))
            .filter(|b| *b <= 256 * 1024 * 1024)
    } else {
        None
    };
    let working_bytes = stripe_bound.unwrap_or(working_bytes);
    let striped = stripe_bound.is_some();
    let name = destination
        .file_name()
        .context("index path has no name")?
        .to_str()
        .context("index path must be UTF-8")?;
    let temporary = parent.join(format!("{name}.partial-{build_id}"));
    fs::create_dir(&temporary)?;
    // Incomplete attempts have zero headers and are never accepted by readers.
    let mut summaries = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(temporary.join("summary.rsi"))?;
    let mut raw = if boundary_source == BoundarySource::Normalized {
        Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(temporary.join("pixels.rsr"))?,
        )
    } else {
        None
    };
    summaries.write_all(&vec![0; HEADER])?;
    if let Some(raw) = &mut raw {
        raw.write_all(&vec![0; HEADER])?;
    }
    let indices: Vec<_> = (0..n).collect();
    let mut calls = 0usize;
    let mut windows_read = 0usize;
    let mut stripe = None;
    for tile in 0..header.tiles() {
        check_cancel(cancel)?;
        let [x, y, x1, y1] = header.bounds(tile);
        let single;
        let r = if striped {
            if x == 0 {
                // Release the previous stripe before allocating the next one.
                drop(stripe.take());
                let (r, metrics) = source.read_selected_window_cancellable(
                    0,
                    y,
                    header.grid.width,
                    y1 - y,
                    &indices,
                    256 * 1024 * 1024 - builder_extra,
                    cancel,
                )?;
                crate::source::validate_window(
                    source.metadata(),
                    &r,
                    [0, y, header.grid.width, y1 - y],
                    &indices,
                )?;
                calls += metrics.raster_io_calls;
                windows_read += 1;
                stripe = Some(r);
            }
            stripe.as_ref().expect("first tile starts a stripe")
        } else {
            let (r, metrics) = source.read_selected_window_cancellable(
                x,
                y,
                x1 - x,
                y1 - y,
                &indices,
                256 * 1024 * 1024 - builder_extra,
                cancel,
            )?;
            crate::source::validate_window(
                source.metadata(),
                &r,
                [x, y, x1 - x, y1 - y],
                &indices,
            )?;
            calls += metrics.raster_io_calls;
            windows_read += 1;
            single = r;
            &single
        };
        let column_offset = if striped { x } else { 0 };
        let mut states = Vec::with_capacity(n * 40);
        for b in &r.bands {
            let mut sum = Sum::default();
            let mut count = 0u64;
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for row in 0..y1 - y {
                check_cancel(cancel)?;
                for col in 0..x1 - x {
                    let pos = row * r.grid.width + column_offset + col;
                    if b.valid[pos] {
                        let v = b.values[pos];
                        sum.add(v);
                        count += 1;
                        min = min.min(v);
                        max = max.max(v);
                    }
                }
            }
            let parts = sum.parts();
            Sum::from_parts(parts)?;
            for v in parts {
                append_f64(&mut states, v);
            }
            states.extend_from_slice(&count.to_le_bytes());
            append_f64(&mut states, if count > 0 { min } else { 0. });
            append_f64(&mut states, if count > 0 { max } else { 0. });
        }
        summaries.write_all(&checksum_page(states))?;
        for g in 0..if raw.is_some() { header.groups() } else { 0 } {
            check_cancel(cancel)?;
            let mut page = Vec::with_capacity(header.raw_size());
            for row in 0..tile_edge {
                for col in 0..tile_edge {
                    let pos = row * r.grid.width + column_offset + col;
                    for slot in 0..group {
                        let bi = g * group + slot;
                        let valid =
                            row < y1 - y && col < x1 - x && bi < n && r.bands[bi].valid[pos];
                        append_f64(&mut page, if valid { r.bands[bi].values[pos] } else { 0. });
                        page.push(u8::from(valid));
                    }
                }
            }
            raw.as_mut()
                .expect("normalized mode")
                .write_all(&checksum_page(page))?;
        }
    }
    drop(stripe);
    // Append parents from checksummed child summaries, never a resampled raster.
    // Only one child record and O(bands) merge state are live at any moment.
    let levels = header.levels();
    let mut parent_reads = 0usize;
    for level in 1..levels.len() {
        let (px, py, poffset) = levels[level];
        let (cx, cy, coffset) = levels[level - 1];
        for y in 0..py {
            for x in 0..px {
                check_cancel(cancel)?;
                let mut sums = vec![Sum::default(); n];
                let mut counts = vec![0u64; n];
                let mut minima = vec![f64::INFINITY; n];
                let mut maxima = vec![f64::NEG_INFINITY; n];
                for yy in y * 2..(y * 2 + 2).min(cy) {
                    for xx in x * 2..(x * 2 + 2).min(cx) {
                        summaries.seek(SeekFrom::Start(
                            (HEADER + (coffset + yy * cx + xx) * header.summary_size()) as u64,
                        ))?;
                        let mut page = vec![0; header.summary_size()];
                        summaries.read_exact(&mut page)?;
                        parent_reads += 1;
                        let data = verify_page(&page)?;
                        for b in 0..n {
                            let p = b * 40;
                            let count = u64::from_le_bytes(data[p + 16..p + 24].try_into()?);
                            sums[b].merge(Sum::from_parts([
                                read_f64(data, p),
                                read_f64(data, p + 8),
                            ])?);
                            counts[b] += count;
                            if count > 0 {
                                minima[b] = minima[b].min(read_f64(data, p + 24));
                                maxima[b] = maxima[b].max(read_f64(data, p + 32));
                            }
                        }
                    }
                }
                let mut states = Vec::with_capacity(n * 40);
                for b in 0..n {
                    let parts = sums[b].parts();
                    Sum::from_parts(parts)?;
                    for v in parts {
                        append_f64(&mut states, v);
                    }
                    states.extend_from_slice(&counts[b].to_le_bytes());
                    append_f64(&mut states, if counts[b] > 0 { minima[b] } else { 0. });
                    append_f64(&mut states, if counts[b] > 0 { maxima[b] } else { 0. });
                }
                summaries.seek(SeekFrom::Start(
                    (HEADER + (poffset + y * px + x) * header.summary_size()) as u64,
                ))?;
                summaries.write_all(&checksum_page(states))?;
            }
        }
    }
    source.verify_immutable()?;
    check_cancel(cancel)?;
    ensure!(
        summaries.metadata()?.len() == index_bytes
            && raw
                .as_ref()
                .map(|f| f.metadata().map(|m| m.len()))
                .transpose()?
                .unwrap_or(0)
                == raw_bytes,
        "persistent output length mismatch"
    );
    summaries.seek(SeekFrom::Start(0))?;
    summaries.write_all(&encode_header(&header)?)?;
    summaries.sync_all()?;
    if let Some(raw) = &mut raw {
        header.kind = "raw".into();
        raw.seek(SeekFrom::Start(0))?;
        raw.write_all(&encode_header(&header)?)?;
        raw.sync_all()?;
    }
    File::open(&temporary)?.sync_all()?;
    check_cancel(cancel)?;
    ensure!(
        !destination.exists(),
        "index destination appeared during build"
    );
    fs::rename(&temporary, destination)?;
    File::open(parent)?.sync_all()?;
    Ok(
        json!({"persistent":true,"boundary_source":boundary_source,"path":destination,"build_id":build_id,"source_id":source.metadata().source_id,"summary_bytes":index_bytes,"duplicate_raw_bytes":raw_bytes,"total_index_and_raw_bytes":index_bytes+raw_bytes,"builder_buffer_bound_bytes":working_bytes,"source_raster_io_calls":calls,"builder_windows_read":windows_read,"builder_read_policy":if striped {"source_stripes"} else {"summary_tiles"},"tile_edge":tile_edge,"band_group":group,"layout":layout,"summary_backend":summary_backend,"summary_levels":levels.len(),"parent_summary_reads":parent_reads,"preparation_ms":started.elapsed().as_secs_f64()*1000.,"atomic_complete":true}),
    )
}

fn signature(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path)?;
    ensure!(m.is_file(), "persistent source must be regular file");
    Ok(format!(
        "{}:{}:{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    ))
}
enum Source {
    Local {
        file: File,
        path: PathBuf,
        signature: String,
        length: u64,
    },
    Remote(RangeSource),
}
impl Source {
    fn open(path: &str) -> Result<Self> {
        ensure!(path.len() <= 4096, "persistent path exceeds budget");
        if path.starts_with("https://") || path.starts_with("http://") {
            Ok(Self::Remote(RangeSource::register(
                path,
                RemoteLimits {
                    max_requests: 4096,
                    max_download_bytes: MAX_QUERY_BYTES,
                    max_range_bytes: RANGE as u64,
                    timeout_seconds: 30,
                },
            )?))
        } else {
            ensure!(
                !path.contains("://") && !path.starts_with("/vsi"),
                "unsupported persistent transport"
            );
            let path = fs::canonicalize(path)?;
            let sig = signature(&path)?;
            let file = File::open(&path)?;
            let length = file.metadata()?.len();
            ensure!(
                signature(&path)? == sig,
                "persistent source changed while opening"
            );
            Ok(Self::Local {
                file,
                path,
                signature: sig,
                length,
            })
        }
    }
    fn length(&self) -> u64 {
        match self {
            Self::Local { length, .. } => *length,
            Self::Remote(s) => s.length,
        }
    }
    fn verify(&self) -> Result<()> {
        if let Self::Local {
            path,
            signature: sig,
            ..
        } = self
        {
            ensure!(
                signature(path)? == *sig,
                "persistent source changed since opening"
            );
        }
        Ok(())
    }
    fn verify_current(&mut self, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        match self {
            Self::Remote(source) => source.verify_remote()?,
            _ => self.verify()?,
        }
        check_cancel(cancel)
    }
    fn read(&mut self, offset: u64, length: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
        check_cancel(cancel)?;
        self.verify()?;
        ensure!(
            length <= 16 * 1024 * 1024
                && offset
                    .checked_add(length as u64)
                    .is_some_and(|end| end <= self.length()),
            "persistent read outside bounds"
        );
        let data = match self {
            Self::Local { file, .. } => {
                file.seek(SeekFrom::Start(offset))?;
                let mut data = vec![0; length];
                file.read_exact(&mut data)?;
                data
            }
            Self::Remote(s) if length <= RANGE => s.read_range(offset, length as u64)?,
            Self::Remote(s) => {
                let mut data = Vec::with_capacity(length);
                for start in (0..length).step_by(RANGE) {
                    check_cancel(cancel)?;
                    data.extend(
                        s.read_range(offset + start as u64, (length - start).min(RANGE) as u64)?,
                    );
                }
                data
            }
        };
        self.verify()?;
        check_cancel(cancel)?;
        Ok(data)
    }
    fn transport(&self) -> Value {
        match self {
            Self::Local { signature, .. } => json!({"kind":"local","identity":signature}),
            Self::Remote(s) => json!({"kind":"http","etag":s.etag,"metrics":s.metrics}),
        }
    }
}
#[derive(Default, Serialize)]
struct Trace {
    index_reads: usize,
    raw_reads: usize,
    index_bytes: u64,
    raw_bytes: u64,
    records: Vec<Value>,
    source_window_reads: usize,
    source_decoded_bytes: usize,
}

/// A validated sufficient state over one complete native rectangle.
/// Min/max are +/-infinity only for an empty state, as expected by Acc.
pub struct TileSummary {
    pub sum: Sum,
    pub valid_count: usize,
    pub min: f64,
    pub max: f64,
}
/// Bounded index access reusable by a tile-driven batch or node-selection plan.
/// It never opens a source named inside metadata and retains no value pages.
pub struct PersistedIndex {
    source: Source,
    header: Header,
    trace: Trace,
    memory: usize,
    invalidated: bool,
}
impl PersistedIndex {
    pub fn open(
        index_path: &str,
        expected_build_id: Option<&str>,
        read_memory_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        ensure!(
            (1024 * 1024..=128 * 1024 * 1024).contains(&read_memory_bytes),
            "index reader memory must be1..128MiB"
        );
        let mut source = Source::open(index_path)?;
        let mut trace = Trace::default();
        let header = decode_header(&trace.read(&mut source, "index", 0, HEADER, None, cancel)?)?;
        ensure!(
            header.kind == "summaries" && header.file_length() == source.length(),
            "truncated or trailing persistent index"
        );
        if let Some(id) = expected_build_id {
            ensure!(id == header.build_id, "persistent build identity mismatch");
        }
        Ok(Self {
            source,
            header,
            trace,
            memory: read_memory_bytes,
            invalidated: false,
        })
    }
    pub fn header(&self) -> &Header {
        &self.header
    }
    /// Conservative retained header, transport and bounded range-log allowance.
    /// No values, decoded windows or summary pages are retained by this handle.
    pub const RETAINED_BYTES: usize = 8 * 1024 * 1024;

    pub fn inspect_retained(&mut self, cancel: &AtomicBool) -> Result<Value> {
        ensure!(
            !self.invalidated,
            "persistent index handle invalidated; close and reopen"
        );
        if let Err(error) = self.source.verify_current(cancel) {
            self.invalidated = true;
            return Err(error);
        }
        Ok(
            json!({"header":self.header,"retained_bound_bytes":Self::RETAINED_BYTES,
            "retained_value_bytes":0,"retained_summary_page_bytes":0,
            "transport":self.source.transport(),"transport_metrics_scope":"handle lifetime"}),
        )
    }

    /// Explicit retained original-source access. Failure invalidates the handle;
    /// callers close/reopen instead of reusing possibly stale transport state.
    pub fn query_original(
        &mut self,
        source: &dyn WindowSource,
        geometry: &Value,
        crs: &str,
        options: &Options,
        forbidden_raw_tiles: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Value> {
        ensure!(
            !self.invalidated,
            "persistent index handle invalidated; close and reopen"
        );
        let q = QueryOptions {
            index_path: "",
            raw_path: "",
            source_path: None,
            expected_build_id: None,
            read_memory_bytes: self.memory,
            summary_page_bytes: None,
            coalesce_raw: true,
            order_summaries: false,
            forbidden_raw_tiles,
            force_direct: false,
            hierarchy: None,
            joint_planner: None,
        };
        let result = query_impl(q, geometry, crs, options, cancel, Some(source), Some(self));
        if result.is_err() {
            self.invalidated = true;
        }
        result
    }
    pub fn verify(&self) -> Result<()> {
        self.source.verify()
    }
    pub fn verify_source(&self, source: &dyn WindowSource) -> Result<()> {
        source.verify_immutable()?;
        self.verify_source_metadata(source)
    }
    /// Only after the caller has verified this source at the current request
    /// boundary. This removes nested metadata probes, not boundary guards.
    pub(crate) fn verify_source_metadata(&self, source: &dyn WindowSource) -> Result<()> {
        self.verify()?;
        ensure!(
            source.metadata().source_id == self.header.source_id
                && serde_json::to_value(source.metadata())? == self.header.source_metadata
                && source.identity_descriptor() == self.header.source_identity,
            "persistent source metadata/identity mismatch"
        );
        Ok(())
    }
    pub fn node_bounds(&self, record: usize) -> Result<[usize; 4]> {
        ensure!(
            record < self.header.summary_records(),
            "summary record out of bounds"
        );
        let levels = self.header.levels();
        let (level, (nx, _, offset)) = levels
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, _, offset))| record >= *offset)
            .context("invalid summary level")?;
        let local = record - offset;
        let edge = self.header.tile_edge << level;
        let (x, y) = (local % nx * edge, local / nx * edge);
        Ok([
            x,
            y,
            (x + edge).min(self.header.grid.width),
            (y + edge).min(self.header.grid.height),
        ])
    }
    pub fn read_leaf_summary(
        &mut self,
        tile: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<TileSummary>> {
        ensure!(tile < self.header.tiles(), "leaf tile out of bounds");
        self.read_summary(tile, cancel)
    }
    pub fn read_summary(&mut self, record: usize, cancel: &AtomicBool) -> Result<Vec<TileSummary>> {
        let [x0, y0, x1, y1] = self.node_bounds(record)?;
        let count = (x1 - x0) * (y1 - y0);
        ensure!(
            2 * HEADER + self.header.summary_size() * 2 <= self.memory,
            "summary exceeds reader memory budget"
        );
        let page = self.trace.read(
            &mut self.source,
            "index",
            HEADER as u64 + (record * self.header.summary_size()) as u64,
            self.header.summary_size(),
            Some(record),
            cancel,
        )?;
        let data = verify_page(&page)?;
        let mut result = Vec::with_capacity(self.header.units.len());
        for band in 0..self.header.units.len() {
            let p = band * 40;
            result.push(crate::stored_summary::validated_state(
                [read_f64(data, p), read_f64(data, p + 8)],
                u64::from_le_bytes(data[p + 16..p + 24].try_into()?),
                read_f64(data, p + 24),
                read_f64(data, p + 32),
                count,
            )?);
        }
        Ok(result)
    }
    pub fn diagnostics(&self) -> Value {
        json!({"io":self.trace,"transport":self.source.transport(),"reader_memory_bytes":self.memory,"retained_value_bytes":0})
    }
}
impl Trace {
    fn read(
        &mut self,
        source: &mut Source,
        kind: &str,
        offset: u64,
        length: usize,
        tile: Option<usize>,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.records.len() + self.source_window_reads < 4096
                && self.index_bytes
                    + self.raw_bytes
                    + self.source_decoded_bytes as u64
                    + length as u64
                    <= MAX_QUERY_BYTES,
            "persistent query IO budget exceeded"
        );
        let data = source.read(offset, length, cancel)?;
        if kind == "index" {
            self.index_reads += 1;
            self.index_bytes += length as u64;
        } else {
            self.raw_reads += 1;
            self.raw_bytes += length as u64;
        }
        self.records
            .push(json!({"source":kind,"offset":offset,"length":length,"tile":tile}));
        Ok(data)
    }
}

#[path = "persistent_joint.rs"]
mod joint_execution;
pub use joint_execution::JointQueryOptions;

pub struct QueryOptions<'a> {
    pub joint_planner: Option<JointQueryOptions>,
    pub index_path: &'a str,
    pub raw_path: &'a str,
    pub source_path: Option<&'a str>,
    pub expected_build_id: Option<&'a str>,
    pub read_memory_bytes: usize,
    pub summary_page_bytes: Option<usize>,
    pub coalesce_raw: bool,
    pub order_summaries: bool,
    pub forbidden_raw_tiles: &'a [usize],
    pub force_direct: bool,
    pub hierarchy: Option<bool>,
}
fn vertex_preflight(input: &Value) -> Result<usize> {
    let coordinates = input["coordinates"]
        .as_array()
        .context("polygon coordinates must be array")?;
    let mut count = 0usize;
    let mut polygon = |rings: &Vec<Value>| -> Result<()> {
        ensure!(rings.len() <= 129, "hole-count budget exceeded");
        for ring in rings {
            count = count.saturating_add(ring.as_array().context("ring must be array")?.len());
            ensure!(count <= MAX_VERTICES, "vertex budget exceeded");
        }
        Ok(())
    };
    match input["type"].as_str() {
        Some("Polygon") => polygon(coordinates)?,
        Some("MultiPolygon") => {
            ensure!(coordinates.len() <= 128, "component-count budget exceeded");
            for p in coordinates {
                polygon(p.as_array().context("polygon coordinates must be array")?)?;
            }
        }
        _ => anyhow::bail!("only GeoJSON Polygon/MultiPolygon supported"),
    }
    Ok(count)
}

/// Normal engine file-query backend. Summary pages are fetched lazily only for
/// eligible whole tiles. Raw pages are fetched once per touched band group.
pub fn query(
    q: QueryOptions<'_>,
    geometry: &Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Value> {
    query_with_boundary_source(q, geometry, crs, options, cancel, None)
}

/// Optional caller-owned reader, useful for additional source adapters and
/// instrumented readers. A path is never loaded from an index manifest.
pub fn query_with_boundary_source(
    q: QueryOptions<'_>,
    geometry: &Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
    boundary_reader: Option<&dyn WindowSource>,
) -> Result<Value> {
    query_impl(q, geometry, crs, options, cancel, boundary_reader, None)
}

#[allow(clippy::too_many_arguments)]
fn query_impl(
    q: QueryOptions<'_>,
    geometry: &Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
    boundary_reader: Option<&dyn WindowSource>,
    retained: Option<&mut PersistedIndex>,
) -> Result<Value> {
    let started = Instant::now();
    check_cancel(cancel)?;
    ensure!(
        (1024 * 1024..=128 * 1024 * 1024).contains(&q.read_memory_bytes),
        "persistent read memory budget must be 1..128 MiB"
    );
    ensure!(
        q.forbidden_raw_tiles.len() <= 65536,
        "forbidden tile diagnostic exceeds budget"
    );
    ensure!(
        matches!(q.summary_page_bytes.unwrap_or(0), 0 | 4096 | 16384 | 65536),
        "summary_page_bytes must be 0, 4096, 16384 or 65536"
    );
    let index_open_started = Instant::now();
    let retained_handle = retained.is_some();
    let mut fresh_index;
    let mut index;
    let cached_header;
    if let Some(retained) = retained {
        ensure!(
            retained.header.boundary_source == BoundarySource::Original,
            "retained index requires original boundary source"
        );
        retained.source.verify_current(cancel)?;
        cached_header = Some(retained.header.clone());
        index = &mut retained.source;
    } else {
        fresh_index = Source::open(q.index_path)?;
        cached_header = None;
        index = &mut fresh_index;
    }
    let index_open_ms = index_open_started.elapsed().as_secs_f64() * 1000.;
    let header_started = Instant::now();
    let mut trace = Trace::default();
    let h = if let Some(header) = cached_header {
        header
    } else {
        decode_header(&trace.read(&mut index, "index", 0, HEADER, None, cancel)?)?
    };
    ensure!(
        h.kind == "summaries" && h.file_length() == index.length(),
        "truncated or trailing persistent index"
    );
    if let Some(id) = q.expected_build_id {
        ensure!(id == h.build_id, "persistent build identity mismatch");
    }
    let index_header_ms = header_started.elapsed().as_secs_f64() * 1000.;
    let source_validation_started = Instant::now();
    if let Some(path) = q.source_path.filter(|_| boundary_reader.is_none()) {
        ensure!(
            crate::io::registration_signature(path)? == h.source_id,
            "persistent index is stale for original source"
        );
    }
    let original = h.boundary_source == BoundarySource::Original;
    let local_original = if original && boundary_reader.is_none() {
        Some(LocalSource::open(q.source_path.context(
            "original boundary source requires explicit path or reader",
        )?)?)
    } else {
        None
    };
    let original_reader: Option<&dyn WindowSource> = if original {
        Some(
            boundary_reader
                .or(local_original.as_ref().map(|s| s as &dyn WindowSource))
                .context("missing original boundary source")?,
        )
    } else {
        ensure!(
            boundary_reader.is_none(),
            "normalized index does not use an original boundary reader"
        );
        None
    };
    if let Some(source) = original_reader {
        source.verify_immutable()?;
        ensure!(
            source.metadata().source_id == h.source_id
                && serde_json::to_value(source.metadata())? == h.source_metadata,
            "persistent original source metadata/identity mismatch"
        );
        ensure!(
            source.identity_descriptor() == h.source_identity,
            "persistent portable identity mismatch"
        );
    }
    let source_open_validation_ms = source_validation_started.elapsed().as_secs_f64() * 1000.;
    options.validate(h.units.len())?;
    let selected_bands = options.selected_bands(h.units.len());
    let mut read_bands = selected_bands.clone();
    if let Some(w) = options.weight_band {
        if !read_bands.contains(&w) {
            read_bands.push(w);
        }
    }
    let order_summaries = q.joint_planner.is_none()
        && q.order_summaries
        && !q.force_direct
        && options.summaries_eligible();
    if q.joint_planner.is_some() {
        ensure!(
            !original,
            "joint planner requires normalized physical pages; original encoded source mapping unavailable"
        );
        ensure!(
            !q.force_direct && options.summaries_eligible(),
            "joint planner requires eligible summary statistics"
        );
        ensure!(
            q.hierarchy != Some(false),
            "joint planner uses the available cover forest; forced flat traversal is a separate control"
        );
    }
    let joint_reservation = q
        .joint_planner
        .map(JointQueryOptions::reserve)
        .transpose()?
        .unwrap_or(0);
    let schedule_limit = h.summary_records().min(crate::range_schedule::MAX_RECORDS);
    let vertices = vertex_preflight(geometry)?;
    let accumulator_bytes = Acc::retained_bytes(options)?
        .checked_mul(selected_bands.len())
        .and_then(|n| n.checked_add(Acc::transient_bytes(options)))
        .context("persistent accumulator budget overflow")?;
    let base_buffers = h.tile_edge * h.tile_edge * (read_bands.len() * 9 + 24)
        + 2 * HEADER
        + 65536
        + if q.raw_path.starts_with("http") {
            65536 + if h.raw_size() > RANGE { RANGE } else { 0 }
        } else {
            0
        }
        + vertices * 8192
        + if order_summaries {
            crate::range_schedule::buffer_bound(schedule_limit)
        } else {
            0
        };
    let base_buffers = base_buffers
        .checked_add(accumulator_bytes)
        .and_then(|v| v.checked_add(joint_reservation))
        .context("persistent buffer budget overflow")?;
    let max_raw_groups = if q.coalesce_raw && q.joint_planner.is_none() {
        (q.read_memory_bytes.saturating_sub(base_buffers).min(RANGE) / h.raw_size())
            .max(1)
            .min(read_bands.len())
            .min(h.groups())
    } else {
        1
    };
    let reader_buffers = if let Some(source) = original_reader {
        source.read_buffer_bound(h.tile_edge, h.tile_edge, &read_bands)?
    } else {
        max_raw_groups * h.raw_size()
    };
    let buffer_bound = base_buffers
        .checked_add(reader_buffers)
        .context("persistent reader memory bound overflow")?;
    ensure!(
        buffer_bound <= q.read_memory_bytes,
        "persistent tile and geometry buffers exceed read memory budget"
    );
    let predicate = GeometryPredicate::new(&h.grid, geometry, crs, cancel)?;
    let mut raw = if original {
        None
    } else {
        let mut raw = Source::open(q.raw_path)?;
        let mut rh = decode_header(&trace.read(&mut raw, "raw", 0, HEADER, None, cancel)?)?;
        ensure!(
            rh.kind == "raw"
                && rh.file_length() == raw.length()
                && rh.build_id == h.build_id
                && rh.grid == h.grid
                && rh.band_group == h.band_group
                && rh.tile_edge == h.tile_edge
                && rh.units == h.units
                && rh.source_id == h.source_id,
            "raw and summary version/grid/layout mismatch"
        );
        rh.kind = "summaries".to_owned();
        ensure!(rh == h, "raw and summary capability/provenance mismatch");
        Some(raw)
    };
    let bins = options.histogram_edges.as_ref().map_or(0, |e| e.len() - 1);
    let mut accumulators: Vec<_> = selected_bands
        .iter()
        .map(|_| Acc::new(bins, options))
        .collect();
    let mut selected = Sum::default();
    let mut intersecting = 0usize;
    let mut full = 0usize;
    let mut boundary = 0usize;
    let mut candidates = 0usize;
    let mut raw_tiles = 0usize;
    let mut raw_cells = 0usize;
    let mut source_windows = Vec::new();
    let mut source_decoded_bytes = 0usize;
    let mut source_calls = 0usize;
    let mut source_read_decode_ms = 0.;
    let mut source_normalization_ms = 0.;
    let eligible = !q.force_direct && options.summaries_eligible();
    let levels = h.levels();
    let remote = matches!(&*index, Source::Remote(_));
    let check_root =
        q.hierarchy.is_none() && eligible && remote && h.algorithm_version == "hierarchy_tiles_v1";
    let whole_grid =
        check_root && predicate.classify([0, 0, h.grid.width, h.grid.height]) == Relation::Inside;
    let hierarchical = q
        .hierarchy
        .unwrap_or(h.algorithm_version == "hierarchy_tiles_v1" && (!remote || whole_grid))
        && eligible;
    let summary_page_bytes =
        q.summary_page_bytes
            .unwrap_or(if remote && !hierarchical { 65536 } else { 0 });
    ensure!(
        !hierarchical || h.algorithm_version == "hierarchy_tiles_v1",
        "persistent hierarchy requires a hierarchical index"
    );
    let [x0, y0, x1, y1] = predicate.bounds();
    let candidate_tiles = (y1.div_ceil(h.tile_edge) - y0 / h.tile_edge)
        .saturating_mul(x1.div_ceil(h.tile_edge) - x0 / h.tile_edge);
    ensure!(
        (if hierarchical {
            h.summary_records()
        } else {
            candidate_tiles
        })
        .saturating_mul(predicate.vertices())
            <= 100_000_000,
        "persistent predicate work budget exceeded"
    );
    let joint_initial_reads = trace.records.len();
    let joint_initial_bytes = trace.index_bytes + trace.raw_bytes;
    let mut joint_execution = q
        .joint_planner
        .map(|config| {
            joint_execution::build(
                &h,
                &predicate,
                config,
                &read_bands,
                selected_bands.len(),
                q.forbidden_raw_tiles,
                &trace,
                cancel,
            )
        })
        .transpose()?;
    if let Some(execution) = &joint_execution {
        candidates = execution.candidates;
    }
    let mut boundary_work = joint_execution.as_ref().map_or(0, |e| e.boundary_work);
    // One bounded read-ahead page, owned only by this query. Record checksums
    // remain per tile. Trace records include every extra summary byte; raw
    // pixel blocks are never speculatively fetched.
    let mut summary_cache = Vec::new();
    let mut summary_cache_first = 0usize;
    let mut summary_cache_hits = 0usize;
    let summaries_per_page = (summary_page_bytes / h.summary_size()).max(1);
    let mut flat_tiles = (y0 / h.tile_edge..y1.div_ceil(h.tile_edge))
        .flat_map(|ty| (x0 / h.tile_edge..x1.div_ceil(h.tile_edge)).map(move |tx| (0, tx, ty)));
    let mut pending = vec![(levels.len() - 1, 0, 0)];
    let mut summary_nodes = 0usize;
    // Bounded two-phase execution: certify once, execute boundary reads, then
    // consume certified summaries in increasing file-offset order. Geometry is
    // immutable; no full-resolution coverage plan or raw interior read is added.
    let mut scheduled = if order_summaries {
        Vec::with_capacity(schedule_limit)
    } else {
        Vec::new()
    };
    let mut ranges = Vec::new();
    let mut scheduled_ranges = 0usize;
    let mut draining = false;
    let mut scheduled_count = 0usize;
    loop {
        let joint_item = joint_execution.as_mut().and_then(|e| e.next());
        let item = if joint_execution.is_some() {
            joint_item.map(|(l, x, y, _)| (l, x, y))
        } else if draining {
            scheduled.pop()
        } else if hierarchical {
            pending.pop()
        } else {
            flat_tiles.next()
        };
        let Some((level, tx, ty)) = item else {
            if order_summaries && !draining {
                scheduled.sort_unstable_by_key(|&(l, x, y)| {
                    std::cmp::Reverse(levels[l].2 + y * levels[l].0 + x)
                });
                scheduled_count = scheduled.len();
                if summary_page_bytes > 0 {
                    let records: Vec<_> = scheduled
                        .iter()
                        .rev()
                        .map(|&(l, x, y)| levels[l].2 + y * levels[l].0 + x)
                        .collect();
                    ranges = crate::range_schedule::plan(&records, summaries_per_page, cancel)?;
                    scheduled_ranges = ranges.len();
                }
                draining = true;
                continue;
            }
            break;
        };
        check_cancel(cancel)?;
        let (nx, _, offset) = levels[level];
        let tile = offset + ty * nx + tx;
        let edge = h.tile_edge << level;
        let bounds = [
            tx * edge,
            ty * edge,
            ((tx + 1) * edge).min(h.grid.width),
            ((ty + 1) * edge).min(h.grid.height),
        ];
        let relation = if let Some((_, _, _, relation)) = joint_item {
            relation
        } else if draining {
            Relation::Inside
        } else {
            candidates += 1;
            predicate.classify(bounds)
        };
        if relation == Relation::Outside {
            continue;
        }
        if level > 0 && relation == Relation::Boundary {
            let (cx, cy, _) = levels[level - 1];
            for yy in (ty * 2..(ty * 2 + 2).min(cy)).rev() {
                for xx in (tx * 2..(tx * 2 + 2).min(cx)).rev() {
                    pending.push((level - 1, xx, yy));
                }
            }
            continue;
        }
        let count = (bounds[2] - bounds[0]) * (bounds[3] - bounds[1]);
        if relation == Relation::Inside && eligible {
            if order_summaries && !draining {
                ensure!(
                    scheduled.len() < schedule_limit,
                    "summary schedule exceeds bounded task capacity"
                );
                scheduled.push((level, tx, ty));
                continue;
            }
            let page = if let Some(execution) = joint_execution.as_mut() {
                execution.read(
                    &mut index,
                    &mut trace,
                    0,
                    HEADER as u64 + (tile * h.summary_size()) as u64,
                    h.summary_size(),
                    tile,
                    cancel,
                )?
            } else if summary_page_bytes == 0 {
                trace.read(
                    &mut index,
                    "index",
                    HEADER as u64 + (tile * h.summary_size()) as u64,
                    h.summary_size(),
                    Some(tile),
                    cancel,
                )?
            } else {
                if tile < summary_cache_first
                    || tile >= summary_cache_first + summary_cache.len() / h.summary_size()
                {
                    let count = if draining {
                        let (first, count) =
                            ranges.pop().context("missing planned summary range")?;
                        ensure!(first == tile, "summary range schedule lost task alignment");
                        summary_cache_first = first;
                        count
                    } else {
                        summary_cache_first = tile / summaries_per_page * summaries_per_page;
                        summaries_per_page.min(h.summary_records() - summary_cache_first)
                    };
                    drop(std::mem::take(&mut summary_cache));
                    summary_cache = trace.read(
                        &mut index,
                        "index",
                        HEADER as u64 + (summary_cache_first * h.summary_size()) as u64,
                        count * h.summary_size(),
                        None,
                        cancel,
                    )?;
                } else {
                    summary_cache_hits += 1;
                }
                let offset = (tile - summary_cache_first) * h.summary_size();
                summary_cache[offset..offset + h.summary_size()].to_vec()
            };
            let data = verify_page(&page)?;
            for (ai, &bi) in selected_bands.iter().enumerate() {
                let p = bi * 40;
                let sum = Sum::from_parts([read_f64(data, p), read_f64(data, p + 8)])?;
                let valid = u64::from_le_bytes(data[p + 16..p + 24].try_into()?) as usize;
                let min = read_f64(data, p + 24);
                let max = read_f64(data, p + 32);
                ensure!(
                    valid <= count
                        && min.is_finite()
                        && max.is_finite()
                        && (valid == 0 || min <= max),
                    "invalid persistent summary state"
                );
                ensure!(
                    valid > 0 || (sum.parts() == [0., 0.] && min == 0. && max == 0.),
                    "empty persistent summary must have zero state"
                );
                if valid > 0 {
                    let mean = sum.value() / valid as f64;
                    let tolerance = 1e-8 + 1e-10 * mean.abs().max(min.abs()).max(max.abs());
                    ensure!(
                        mean >= min - tolerance && mean <= max + tolerance,
                        "persistent summary sum is inconsistent with extrema/count"
                    );
                }
                accumulators[ai].merge_summary(
                    sum,
                    valid,
                    if valid > 0 { min } else { f64::INFINITY },
                    if valid > 0 { max } else { f64::NEG_INFINITY },
                );
            }
            selected.add(count as f64);
            intersecting += count;
            full += (bounds[2].div_ceil(h.tile_edge) - bounds[0] / h.tile_edge)
                * (bounds[3].div_ceil(h.tile_edge) - bounds[1] / h.tile_edge);
            summary_nodes += 1;
            continue;
        }
        boundary_work = boundary_work.saturating_add(count.saturating_mul(predicate.vertices()));
        ensure!(
            boundary_work <= 100_000_000,
            "persistent boundary work budget exceeded"
        );
        let cells = predicate.boundary_cells(bounds, cancel)?;
        if cells.is_empty() {
            continue;
        }
        ensure!(
            !q.forbidden_raw_tiles.contains(&tile),
            "forbidden raw interior tile read: {tile}"
        );
        let mut bands: Vec<Band> = read_bands
            .iter()
            .map(|_| Band {
                values: vec![0.; h.tile_edge * h.tile_edge],
                valid: vec![false; h.tile_edge * h.tile_edge],
                unit: None,
            })
            .collect();
        if let Some(source) = original_reader {
            let width = bounds[2] - bounds[0];
            let height = bounds[3] - bounds[1];
            let bytes = width * height * read_bands.len() * 9;
            ensure!(
                source_windows.len() + trace.records.len() < 4096
                    && source_decoded_bytes as u64 + trace.index_bytes + bytes as u64
                        <= MAX_QUERY_BYTES,
                "original boundary query IO budget exceeded"
            );
            let (raster, metrics) = source.read_selected_window_cancellable(
                bounds[0],
                bounds[1],
                width,
                height,
                &read_bands,
                q.read_memory_bytes.saturating_sub(base_buffers),
                cancel,
            )?;
            crate::source::validate_window(
                source.metadata(),
                &raster,
                [bounds[0], bounds[1], width, height],
                &read_bands,
            )?;
            for (target, source_band) in bands.iter_mut().zip(&raster.bands) {
                for row in 0..height {
                    target.values[row * h.tile_edge..row * h.tile_edge + width]
                        .copy_from_slice(&source_band.values[row * width..(row + 1) * width]);
                    target.valid[row * h.tile_edge..row * h.tile_edge + width]
                        .copy_from_slice(&source_band.valid[row * width..(row + 1) * width]);
                }
            }
            source_decoded_bytes += bytes;
            trace.source_decoded_bytes = source_decoded_bytes;
            trace.source_window_reads += 1;
            source_calls += metrics.raster_io_calls;
            source_read_decode_ms += metrics.read_decode_ms;
            source_normalization_ms += metrics.normalization_ms;
            source_windows.push(json!({"tile":tile,"window":[bounds[0],bounds[1],width,height],"bands":read_bands,"decoded_bytes":bytes}));
        } else {
            let mut groups: Vec<_> = read_bands.iter().map(|bi| bi / h.band_group).collect();
            groups.sort_unstable();
            groups.dedup();
            let mut at = 0;
            while at < groups.len() {
                let first = groups[at];
                let mut end = at + 1;
                while end < groups.len()
                    && end - at < max_raw_groups
                    && groups[end] == groups[end - 1] + 1
                {
                    end += 1;
                }
                let page_index = tile * h.groups() + first;
                let page = if let Some(execution) = joint_execution.as_mut() {
                    execution.read(
                        raw.as_mut().expect("normalized mode"),
                        &mut trace,
                        1,
                        HEADER as u64 + (page_index * h.raw_size()) as u64,
                        h.raw_size() * (end - at),
                        tile,
                        cancel,
                    )?
                } else {
                    trace.read(
                        raw.as_mut().expect("normalized mode"),
                        "raw",
                        HEADER as u64 + (page_index * h.raw_size()) as u64,
                        h.raw_size() * (end - at),
                        Some(tile),
                        cancel,
                    )?
                };
                for (slot, &group) in groups[at..end].iter().enumerate() {
                    let data = verify_page(&page[slot * h.raw_size()..(slot + 1) * h.raw_size()])?;
                    for (j, &bi) in read_bands
                        .iter()
                        .enumerate()
                        .filter(|(_, bi)| **bi / h.band_group == group)
                    {
                        for pos in 0..h.tile_edge * h.tile_edge {
                            if pos % 4096 == 0 {
                                check_cancel(cancel)?;
                            }
                            let offset = (pos * h.band_group + bi % h.band_group) * 9;
                            let v = read_f64(data, offset);
                            let valid = data[offset + 8];
                            ensure!(
                                valid <= 1 && v.is_finite(),
                                "invalid normalized persistent raw value/mask"
                            );
                            bands[j].values[pos] = v;
                            bands[j].valid[pos] = valid == 1;
                        }
                    }
                }
                at = end;
            }
        }
        let weights = options.weight_band.map(|bi| {
            &bands[read_bands
                .iter()
                .position(|&x| x == bi)
                .expect("included weight")]
        });
        for cell in cells {
            let pos = (cell.row - bounds[1]) * h.tile_edge + cell.col - bounds[0];
            selected.add(cell.fraction);
            intersecting += 1;
            raw_cells += 1;
            for (ai, acc) in accumulators.iter_mut().enumerate() {
                acc.cell(
                    &bands[ai],
                    pos,
                    cell.fraction,
                    weights,
                    options.histogram_edges.as_deref(),
                );
            }
        }
        raw_tiles += 1;
        if relation == Relation::Boundary {
            boundary += 1;
        }
    }
    if let Some(execution) = &joint_execution {
        execution.finish(&trace, joint_initial_reads, joint_initial_bytes)?;
    }
    if retained_handle {
        index.verify_current(cancel)?;
    } else {
        index.verify()?;
    }
    if let Some(raw) = &raw {
        raw.verify()?;
    }
    if let Some(source) = original_reader {
        source.verify_immutable()?;
    }
    if let Some(path) = q.source_path.filter(|_| boundary_reader.is_none()) {
        ensure!(
            crate::io::registration_signature(path)? == h.source_id,
            "original source changed during indexed query"
        );
    }
    let bands = accumulators
        .into_iter()
        .zip(&selected_bands)
        .map(|(a, &bi)| {
            a.finish_cancellable(
                bi,
                selected.value(),
                predicate.polygon_area(),
                intersecting,
                h.units[bi].clone(),
                options,
                cancel,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let strategy = if q.force_direct {
        "persistent_raw_scan"
    } else if hierarchical {
        "persistent_hierarchy"
    } else if eligible {
        "persistent_flat"
    } else {
        "persistent_combined_raw"
    };
    let mut result = json!({"bands":bands,"grid":h.grid,"source_id":h.source_id,"build_id":h.build_id,"mode":"native_grid_planar","precision":"f64_compensated","strategy":strategy,"persistent":true,"plan_reused":false,"selection_reason":if q.force_direct {"forced direct ablation"} else if eligible {"sum/support/extrema supported by persisted tile summaries"} else {"arbitrary histogram or joint weight fields absent from index; one shared geometry/raw pass"},"work":{"candidate_tiles":candidates,"summary_tiles":full,"raw_tiles":raw_tiles,"boundary_tiles":boundary,"raw_positive_cells":raw_cells,"eligible_raw_interior_tiles_avoided":full,"buffer_bound_bytes":buffer_bound,"read_memory_budget_bytes":q.read_memory_bytes,"index_total_bytes":index.length(),"duplicate_raw_total_bytes":raw.as_ref().map_or(0,Source::length),"application_cache_bytes":0,"full_resolution_plan":false},"io":trace,"transports":{"index":index.transport(),"raw":raw.as_ref().map(Source::transport)},"timing_ms":{"total":started.elapsed().as_secs_f64()*1000.}});
    result["boundary_source"] = json!(h.boundary_source);
    result["original_source_access"] = original_reader.map_or(Value::Null, |s| s.diagnostics());
    result["source_io"] = json!({"windows":source_windows,"decoded_bytes":source_decoded_bytes,
        "adapter_calls":source_calls,"read_decode_ms":source_read_decode_ms,"normalization_ms":source_normalization_ms,
        "physical_bytes":null,"physical_blocks":null,
        "scope":"requested native windows; adapter physical block cache and overread are separate"});
    result["work"]["application_cache_bytes"] = json!(summary_cache.len());
    result["work"]["summary_page_bytes"] = json!(summary_page_bytes);
    result["work"]["coalesce_raw"] = json!(q.coalesce_raw);
    result["work"]["maximum_raw_groups_per_read"] = json!(max_raw_groups);
    result["work"]["planner_predicate_calls"] = json!(usize::from(check_root));
    result["selection_reason"] = json!(if q.force_direct {
        "forced raw-leaf ablation"
    } else if !eligible {
        "joint statistics absent from index; shared geometry and raw pass"
    } else if q.hierarchy.is_some() {
        "forced persistent traversal"
    } else if hierarchical {
        "prepared hierarchy; local source or certified whole-grid query"
    } else if remote {
        "remote partial query; flat summaries with bounded read-ahead reduce round trips"
    } else {
        "available flat summaries"
    });
    result["work"]["order_summaries"] = json!(order_summaries);
    result["work"]["scheduled_ranges"] = json!(scheduled_ranges);
    result["work"]["scheduled_summaries"] = json!(scheduled_count);
    result["work"]["summary_cache_hits"] = json!(summary_cache_hits);
    result["work"]["summary_nodes"] = json!(summary_nodes);
    result["work"]["hierarchy_levels"] = json!(if hierarchical { levels.len() } else { 1 });
    if let Some(execution) = &joint_execution {
        result["strategy"] = json!("persistent_joint_cover_ranges");
        result["selection_reason"] =
            json!("explicit bounded coupled cover and physical-range optimization");
        result["joint_planner"] = execution.diagnostics();
        result["joint_planner"]["fixed_header_requests"] = json!(joint_initial_reads);
        result["joint_planner"]["fixed_header_bytes"] = json!(joint_initial_bytes);
        result["work"]["hierarchy_levels"] = json!(levels.len());
        result["work"]["joint_reserved_bytes"] = json!(joint_reservation);
    }
    options.project(&mut result);
    result["retained_index_handle"] = json!(retained_handle);
    result["index_transport_metrics_scope"] = json!(if retained_handle {
        "handle lifetime"
    } else {
        "query"
    });
    let total_ms = started.elapsed().as_secs_f64() * 1000.;
    result["timing_ms"] = json!({
        "total": total_ms,
        "index_open_or_revalidate": index_open_ms,
        "index_header_read_validation": index_header_ms,
        "source_open_validation": source_open_validation_ms,
        "remaining_execution_assembly": total_ms - index_open_ms - index_header_ms - source_open_validation_ms,
        "scope": "exclusive partition of this query call; source post-validation is in remaining execution; FFI envelope and transport subspans are not added to this partition"
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_header_mutations_never_panic_or_allocate_from_untrusted_dimensions() {
        let base = json!({"version":1,"kind":"summaries","numerical_contract":"native_grid_planar_v1","algorithm_version":"hierarchy_tiles_v1",
            "grid":{"width":17,"height":19,"transform":[0.,1.,0.,19.,0.,-1.],"crs":"LOCAL"},
            "source_id":"fixture","source_metadata":{},"build_id":"a".repeat(64),"units":[null],"tile_edge":16,"band_group":1,
            "capabilities":CAPABILITIES,"byte_order":"little","scalar":"normalized_f64_u8_valid"});
        let bytes = |value: &Value| {
            let json = serde_json::to_vec(value).unwrap();
            let mut data = vec![0; HEADER - 32];
            data[..8].copy_from_slice(MAGIC);
            data[8..16].copy_from_slice(&(json.len() as u64).to_le_bytes());
            data[16..16 + json.len()].copy_from_slice(&json);
            checksum_page(data)
        };
        assert!(decode_header(&bytes(&base)).is_ok());
        let values = [
            json!(null),
            json!(true),
            json!(-1),
            json!(0),
            json!(3),
            json!(u64::MAX),
            json!("unexpected"),
            json!([]),
            json!({}),
        ];
        for field in [
            "version",
            "kind",
            "algorithm_version",
            "tile_edge",
            "band_group",
            "units",
            "capabilities",
            "byte_order",
            "scalar",
        ] {
            for value in &values {
                let mut h = base.clone();
                h[field] = value.clone();
                let data = bytes(&h);
                assert!(
                    std::panic::catch_unwind(|| decode_header(&data)).is_ok(),
                    "panic for {field}: {value}"
                );
                assert!(
                    decode_header(&data).is_err(),
                    "unexpected acceptance for {field}: {value}"
                );
            }
        }
        for field in ["width", "height"] {
            for value in [json!(0), json!(u64::MAX), json!(-1), json!(null)] {
                let mut h = base.clone();
                h["grid"][field] = value;
                let data = bytes(&h);
                assert!(std::panic::catch_unwind(|| decode_header(&data)).is_ok());
                assert!(decode_header(&data).is_err());
            }
        }
        let original = bytes(&base);
        for n in [0, 1, 7, 8, 15, 16, 31, 32, HEADER - 1] {
            assert!(decode_header(&original[..n]).is_err());
        }
    }
    #[test]
    fn checksum_detects_corruption_and_truncation() {
        let page = checksum_page(vec![0; 80]);
        assert!(verify_page(&page).is_ok());
        let mut bad = page.clone();
        bad[15] = 1;
        assert!(verify_page(&bad).is_err());
        assert!(verify_page(&page[..page.len() - 1]).is_err());
        assert!(decode_header(&vec![0; HEADER]).is_err());
    }
    #[test]
    fn serialized_compensation_preserves_residual() {
        let mut s = Sum::default();
        s.add(1e16);
        s.add(1.);
        let roundtrip = Sum::from_parts(s.parts()).unwrap();
        let mut total = Sum::default();
        total.merge(roundtrip);
        total.add(-1e16);
        assert_eq!(total.value(), 1.);
        assert!(Sum::from_parts([f64::NAN, 0.]).is_err());
    }
}
