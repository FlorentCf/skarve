//! Native bounded polygon x raster execution. Geometry is reusable; validity is not.
use crate::{
    aggregate::{Acc, NumericBand, Options},
    coverage::{self, Cell, Span},
    expression::Expr,
    model::{Band, Grid, Raster, Sum, check_cancel},
    persistent::PersistedIndex,
    shared_rows::RowSummary,
    source::{BandMetadata, RasterMetadata, ReadMetrics, SourceSpec, WindowSource},
    tile_cache::TileCache,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    pub id: String,
    pub version: String,
    pub geometry: Value,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    pub id: String,
    pub time: Option<String>,
    pub variable: Option<String>,
    /// A registered resident raster or reader handle.
    pub source: Option<String>,
    /// Alternatively a lazily opened native source (one active slice at a time).
    pub spec: Option<SourceSpec>,
    pub index: Option<String>,
    pub expected_build_id: Option<String>,
    pub bands: Option<Vec<usize>>,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    Feature,
    #[default]
    Tile,
    Mixed,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GeometryLayout {
    #[default]
    Auto,
    Csr,
    Compact,
    CompactShared,
    CompactRows,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    Full,
    Numeric,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WindowPolicy {
    #[default]
    Fixed,
    SourceLayout,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    pub working_bytes: usize,
    pub geometry_bytes: usize,
    pub tile_bytes: usize,
    pub output_bytes: usize,
    pub max_windows: usize,
    pub decoded_bytes: u64,
    pub max_contributions: usize,
    pub workers: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            working_bytes: 512 << 20,
            geometry_bytes: 128 << 20,
            tile_bytes: 64 << 20,
            output_bytes: 16 << 20,
            max_windows: 16384,
            decoded_bytes: 2 << 30,
            max_contributions: 1_000_000_000,
            workers: 1,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub zones: Vec<Zone>,
    pub slices: Vec<Slice>,
    pub crs: String,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub expression: Option<Expr>,
    #[serde(default)]
    pub mask: Option<Expr>,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub geometry_layout: GeometryLayout,
    #[serde(default = "default_edge")]
    pub tile_edge: usize,
    #[serde(default)]
    pub window_policy: WindowPolicy,
    #[serde(default)]
    pub output_mode: OutputMode,
    #[serde(default)]
    pub budget: Budget,
}
fn default_edge() -> usize {
    256
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub source_id: String,
    pub grid_id: String,
    pub index_build_id: Option<String>,
}
// Source IDs retain the existing 1024-byte API limit. Grid/build identities
// are native 64-byte digests. Bound supplied pins before retaining any clone.
fn pin_is_bounded(pin: &Pin) -> bool {
    pin.source_id.len() <= 1024
        && pin.grid_id.len() <= 64
        && pin.index_build_id.as_ref().is_none_or(|id| id.len() <= 64)
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u32,
    pub fingerprint: String,
    pub next_row: usize,
    pub pins: Vec<Option<Pin>>,
}

#[derive(Default)]
struct Selection {
    zone: usize,
    spans: Vec<Span>,
    cells: Vec<Cell>,
    whole: bool,
}
struct Piece {
    row: usize,
    start: usize,
    end: usize,
    fraction: f64,
    zones: Vec<usize>,
}
#[derive(Default)]
struct Tile {
    selections: Vec<Selection>,
    shared: Vec<Piece>,
    rows: BTreeMap<usize, Vec<(usize, usize)>>,
}
struct ZoneMeasure {
    selected: f64,
    area: f64,
    intersecting: usize,
}
struct Geometry {
    grid: Grid,
    edge: usize,
    zones: Vec<ZoneMeasure>,
    tiles: BTreeMap<(usize, usize), Tile>,
    bytes: usize,
}
#[derive(Default, Serialize)]
pub struct Metrics {
    pub geometry_compilations: usize,
    pub geometry_cache_hits: usize,
    pub source_opens: usize,
    pub source_acquisitions: usize,
    pub lazy_source_cache_hits: usize,
    pub windows_read: usize,
    pub decoded_cache_hits: usize,
    pub decoded_cache_hit_payload_bytes: u64,
    pub decoded_cache_admitted_payload_bytes: u64,
    pub decoded_cache_evictions: u64,
    pub decoded_cache_admission_rejections: u64,
    pub decoded_cache_resident_bytes: usize,
    pub decoded_cache_budget_bytes: usize,
    pub decoded_value_bytes: u64,
    pub raw_band_cell_visits: u64,
    pub shared_range_reductions: usize,
    pub row_prefix_builds: usize,
    pub row_prefix_rejections: usize,
    pub row_prefix_input_visits: u64,
    pub row_prefix_ranges: usize,
    pub row_range_fallbacks: usize,
    pub row_direct_spans: usize,
    pub summary_records_read: usize,
    pub source_summary_records_read: usize,
    pub band_group_reads: usize,
    pub summarized_polygon_tiles: usize,
    pub geometry_bytes: usize,
    pub accumulator_reserved_bytes: usize,
    pub accumulator_states: usize,
    pub reducer_identity_builds: usize,
    pub numeric_results_direct: usize,
    pub checkpoint_reserved_bytes: usize,
    pub peak_tracked_bytes: usize,
    pub output_rows: usize,
    pub network_requests: u64,
    pub network_body_bytes: u64,
    pub network_get_requests: u64,
    pub network_head_requests: u64,
    pub transfer_cache_hits: u64,
    pub job_setup_ms: f64,
    pub source_acquisition_ms: f64,
    pub verification_ms: f64,
    pub grid_hash_ms: f64,
    pub result_assembly_ms: f64,
    pub output_accounting_ms: f64,
    pub checkpoint_ms: f64,
    pub page_assembly_ms: f64,
    pub next_ms: f64,
    pub window_policy_ms: f64,
    pub window_policy_promotions: usize,
    pub window_policy_fallbacks: usize,
    pub last_window_policy: Option<Value>,
    pub geometry_layout_digest: String,
    pub geometry_tasks: usize,
    pub compilation_ms: f64,
    pub io_ms: f64,
    pub read_decode_ms: f64,
    pub normalization_ms: f64,
    pub read_window_validation_ms: f64,
    pub raster_io_calls: usize,
    pub decoded_cache_ms: f64,
    pub expression_ms: f64,
    /// Mask-only work in completed evaluation phases; partial phases excluded.
    /// A later window/query failure does not undo an already completed phase.
    pub mask_expression_cells: u64,
    pub mask_expression_passes: u64,
    pub shared_mask_peak_bytes: usize,
    pub reduction_ms: f64,
}
struct LazyReader {
    // The immutable JobSpec already owns and accounts for the complete key.
    // Indexing it avoids duplicating or serializing transport credentials.
    spec_index: usize,
    source: Box<dyn WindowSource>,
}

pub struct Job {
    spec: JobSpec,
    fingerprint: String,
    pins: Vec<Option<Pin>>,
    cursor: usize,
    geometries: BTreeMap<String, Geometry>,
    ready: VecDeque<(Value, usize)>,
    ready_bytes: usize,
    descriptor: Option<Value>,
    descriptor_charge: usize,
    spec_bytes: usize,
    checkpoint_reserve: usize,
    pub metrics: Metrics,
    resume_check: bool,
    lazy_reader: Option<LazyReader>,
}

fn bounded_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024
}
impl Job {
    pub fn new(
        spec: JobSpec,
        checkpoint: Option<Checkpoint>,
        spec_bytes: usize,
        available: usize,
    ) -> Result<Self> {
        let setup_start = Instant::now();
        ensure!(
            (1..=4096).contains(&spec.zones.len()) && (1..=4096).contains(&spec.slices.len()),
            "batch requires 1..4096 zones and slices"
        );
        ensure!(bounded_id(&spec.crs), "invalid batch CRS");
        let mut ids = BTreeSet::new();
        for z in &spec.zones {
            ensure!(
                bounded_id(&z.id) && bounded_id(&z.version) && ids.insert(&z.id),
                "zone IDs must be unique with nonempty version"
            );
        }
        ids.clear();
        for s in &spec.slices {
            ensure!(
                bounded_id(&s.id) && ids.insert(&s.id),
                "slice IDs must be unique"
            );
            ensure!(
                s.source.is_some() != s.spec.is_some(),
                "slice requires exactly one source handle or spec"
            );
            ensure!(
                s.source.as_ref().is_none_or(|v| bounded_id(v))
                    && s.time.as_ref().is_none_or(|v| bounded_id(v))
                    && s.variable.as_ref().is_none_or(|v| bounded_id(v)),
                "slice metadata exceeds budget"
            );
            ensure!(
                s.index.as_ref().is_none_or(|v| v.len() <= 4096),
                "index location too long"
            );
            ensure!(
                s.expected_build_id.is_none() || s.index.is_some(),
                "expected_build_id requires index"
            );
        }
        ensure!(
            [32, 64, 128, 256, 512].contains(&spec.tile_edge),
            "unsupported batch tile edge"
        );
        if spec.output_mode == OutputMode::Numeric {
            let stats: BTreeSet<_> = spec
                .options
                .statistics
                .as_ref()
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            ensure!(
                stats == BTreeSet::from(["sum", "support", "mean", "min", "max", "count"])
                    && spec.options.weight_band.is_none()
                    && !spec.options.has_new_reducers()
                    && spec.options.histogram_edges.is_none(),
                "numeric output requires exactly sum/support/mean/min/max/count without weighted or extended reducers"
            );
        }
        let checkpoint_reserve = spec.slices.len().saturating_mul(32 << 10);
        let b = &spec.budget;
        ensure!(
            b.workers == 1,
            "batch currently supports exactly one native worker per job"
        );
        ensure!(
            (1 << 20..=1 << 30).contains(&b.working_bytes) && b.working_bytes <= available,
            "batch working budget exceeds available session memory"
        );
        ensure!(
            b.geometry_bytes > 0
                && b.tile_bytes > 0
                && b.output_bytes > 0
                && b.output_bytes <= 32 << 20
                && b.max_windows > 0
                && b.max_windows <= 1_000_000
                && b.decoded_bytes > 0
                && b.max_contributions > 0
                && b.max_contributions <= 4_000_000_000,
            "invalid batch budget"
        );
        ensure!(
            b.geometry_bytes
                .saturating_add(b.tile_bytes)
                .saturating_add(b.output_bytes)
                .saturating_add(spec_bytes)
                .saturating_add(checkpoint_reserve)
                < b.working_bytes,
            "batch subdivisions exceed working budget"
        );
        // Declared interpretation/content pins belong to the job even before a lazy
        // slice is opened. Transport locations, credentials and verification policy
        // are deliberately excluded; first-use pins still verify actual sources.
        let identity = json!({"zones":spec.zones.iter().map(|z|json!({"id":z.id,"version":z.version,"geometry":z.geometry})).collect::<Vec<_>>(),
            "slices":spec.slices.iter().map(|s| {
                let mut slice = json!({"id":s.id,"time":s.time,"variable":s.variable,"bands":s.bands});
                if let Some(source) = &s.spec {
                    slice["declared_source"] = json!({
                        "format":source.format,"variable":source.variable,"crs":source.crs,
                        "longitude_shift":source.longitude_shift,"bands":source.bands,
                        "identity":source.identity.as_ref().map(|identity| json!({
                            "sha256":identity.sha256.to_ascii_lowercase(),"byte_length":identity.byte_length
                        }))
                    });
                }
                slice
            }).collect::<Vec<_>>(),
            "crs":spec.crs,"options":spec.options,"expression":spec.expression,"mask":spec.mask});
        let fingerprint = blake3::hash(&serde_json::to_vec(&identity)?)
            .to_hex()
            .to_string();
        let mut pins = vec![None; spec.slices.len()];
        let mut cursor = 0;
        let mut resume_check = false;
        if let Some(c) = checkpoint {
            ensure!(
                c.version == 1
                    && c.fingerprint == fingerprint
                    && c.pins.len() == pins.len()
                    && c.next_row <= spec.zones.len() * spec.slices.len(),
                "checkpoint incompatible with job"
            );
            ensure!(
                c.pins
                    .iter()
                    .take(c.next_row.div_ceil(spec.zones.len()))
                    .all(Option::is_some),
                "checkpoint missing committed source pins"
            );
            ensure!(
                c.pins.iter().flatten().all(pin_is_bounded),
                "checkpoint pin identifier exceeds source or digest bounds"
            );
            pins = c.pins;
            cursor = c.next_row;
            resume_check = true;
        }
        Ok(Self {
            spec,
            fingerprint,
            pins,
            cursor,
            geometries: BTreeMap::new(),
            ready: VecDeque::new(),
            ready_bytes: 0,
            descriptor: None,
            descriptor_charge: 0,
            spec_bytes,
            checkpoint_reserve,
            metrics: Metrics {
                checkpoint_reserved_bytes: checkpoint_reserve,
                job_setup_ms: setup_start.elapsed().as_secs_f64() * 1000.,
                ..Metrics::default()
            },
            resume_check,
            lazy_reader: None,
        })
    }
    pub fn reservation(&self) -> usize {
        self.spec.budget.working_bytes
    }
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            version: 1,
            fingerprint: self.fingerprint.clone(),
            next_row: self.cursor,
            pins: self.pins.clone(),
        }
    }
    pub fn info(&self) -> Value {
        json!({"fingerprint":self.fingerprint,"total_rows":self.spec.zones.len()*self.spec.slices.len(),"next_row":self.cursor,"schedule":self.spec.schedule,"geometry_layout":self.spec.geometry_layout,"budget":self.spec.budget,"checkpoint":self.checkpoint()})
    }

    /// Compatibility entry point: every page carries a resumable checkpoint.
    pub fn next<'a, F>(&mut self, max_rows: usize, open: F, cancel: &AtomicBool) -> Result<Value>
    where
        F: FnMut(&Slice) -> Result<Box<dyn WindowSource + 'a>>,
    {
        self.next_with_checkpoint(max_rows, true, open, cancel)
    }

    /// Suppression skips cloning/serializing the growing pin vector. Final pages
    /// always include their checkpoint; `info` can snapshot one without IO.
    pub fn next_with_checkpoint<'a, F>(
        &mut self,
        max_rows: usize,
        include_checkpoint: bool,
        open: F,
        cancel: &AtomicBool,
    ) -> Result<Value>
    where
        F: FnMut(&Slice) -> Result<Box<dyn WindowSource + 'a>>,
    {
        self.next_with_cache(
            max_rows,
            include_checkpoint,
            open,
            cancel,
            &mut TileCache::default(),
        )
    }

    /// Session cache capacity is reserved once by the caller outside each job's
    /// working reservation. Exclusive borrowing serializes lookup/read/admission
    /// and supplies single-flight execution without a second worker cache.
    pub fn next_with_cache<'a, F>(
        &mut self,
        max_rows: usize,
        include_checkpoint: bool,
        mut open: F,
        cancel: &AtomicBool,
        cache: &mut TileCache,
    ) -> Result<Value>
    where
        F: FnMut(&Slice) -> Result<Box<dyn WindowSource + 'a>>,
    {
        let next_start = Instant::now();
        ensure!(
            (1..=4096).contains(&max_rows),
            "page rows must be in 1..4096"
        );
        check_cancel(cancel)?;
        if self.resume_check {
            // Every committed pin is still verified, even when its reader reuses
            // the preceding slice's identical source specification.
            for i in 0..self.pins.len() {
                let Some(pin) = self.pins[i].clone() else {
                    continue;
                };
                if self.spec.slices[i].spec.is_some() {
                    let (cached, before) = self.take_lazy_reader(i, cancel)?;
                    let result = self.verify_resume_pin(i, cached.source.as_ref(), &pin, cancel);
                    self.record_source_access(&before, &cached.source.diagnostics());
                    if result.is_ok() {
                        self.lazy_reader = Some(cached);
                    }
                    result?;
                } else {
                    // Avoid retaining an unrelated owned reader alongside a
                    // factory-borrowed resident/registered source.
                    self.lazy_reader = None;
                    let source = open(&self.spec.slices[i])?;
                    self.metrics.source_acquisitions += 1;
                    let before = source.diagnostics();
                    let result = self.verify_resume_pin(i, source.as_ref(), &pin, cancel);
                    self.record_source_access(&before, &source.diagnostics());
                    result?;
                }
            }
            self.resume_check = false;
        }
        if self.ready.is_empty() && self.cursor == self.spec.zones.len() * self.spec.slices.len() {
            // Completed-page polling is idempotent, including accumulated metrics.
            let mut page = json!({"rows":[],"complete":true,"metrics":self.metrics,
                "buffered_rows":0,"buffered_output_bytes":self.ready_bytes,"checkpoint":self.checkpoint()});
            if let Some(descriptor) = &self.descriptor {
                page["descriptor"] = descriptor.clone();
            }
            return Ok(page);
        }
        if self.ready.is_empty() && self.cursor < self.spec.zones.len() * self.spec.slices.len() {
            let slice_index = self.cursor / self.spec.zones.len();
            let skip = self.cursor % self.spec.zones.len();
            let acquisition_start = Instant::now();
            let rows = if self.spec.slices[slice_index].spec.is_some() {
                let (cached, before) = self.take_lazy_reader(slice_index, cancel)?;
                self.metrics.source_acquisition_ms +=
                    acquisition_start.elapsed().as_secs_f64() * 1000.;
                let result = self
                    .run_slice(slice_index, cached.source.as_ref(), cancel, cache)
                    .and_then(|rows| {
                        check_cancel(cancel)?;
                        Ok(rows)
                    });
                self.record_source_access(&before, &cached.source.diagnostics());
                if result.is_ok() {
                    self.lazy_reader = Some(cached);
                }
                result?
            } else {
                self.lazy_reader = None;
                let source = open(&self.spec.slices[slice_index])?;
                self.metrics.source_acquisitions += 1;
                let before = source.diagnostics();
                self.metrics.source_acquisition_ms +=
                    acquisition_start.elapsed().as_secs_f64() * 1000.;
                let result = self.run_slice(slice_index, source.as_ref(), cancel, cache);
                self.record_source_access(&before, &source.diagnostics());
                result?
            };
            // No partial successful slice is published after any reduction/read failure.
            let accounting_start = Instant::now();
            self.ready = rows.into_iter().skip(skip).collect();
            self.ready_bytes = self
                .ready
                .iter()
                .map(|(_, charge)| *charge)
                .sum::<usize>()
                .saturating_add(self.descriptor_charge);
            self.metrics.output_accounting_ms += accounting_start.elapsed().as_secs_f64() * 1000.;
            ensure!(
                self.ready_bytes <= self.spec.budget.output_bytes,
                "batch output slice exceeds output budget"
            );
        }
        let mut rows = Vec::new();
        while rows.len() < max_rows {
            if let Some((row, charge)) = self.ready.pop_front() {
                let accounting_start = Instant::now();
                self.ready_bytes = self.ready_bytes.saturating_sub(charge);
                self.metrics.output_accounting_ms +=
                    accounting_start.elapsed().as_secs_f64() * 1000.;
                rows.push(row);
                self.cursor += 1;
                self.metrics.output_rows += 1;
            } else {
                break;
            }
        }
        let complete = self.cursor == self.spec.zones.len() * self.spec.slices.len();
        let page_start = Instant::now();
        let mut page = json!({"complete":complete,
            "buffered_rows":self.ready.len(),"buffered_output_bytes":self.ready_bytes});
        page["rows"] = Value::Array(rows);
        if let Some(descriptor) = &self.descriptor {
            page["descriptor"] = descriptor.clone();
        }
        self.metrics.page_assembly_ms += page_start.elapsed().as_secs_f64() * 1000.;
        if include_checkpoint || complete {
            let checkpoint_start = Instant::now();
            page["checkpoint"] = serde_json::to_value(self.checkpoint())?;
            self.metrics.checkpoint_ms += checkpoint_start.elapsed().as_secs_f64() * 1000.;
        }
        self.metrics.next_ms += next_start.elapsed().as_secs_f64() * 1000.;
        page["metrics"] = serde_json::to_value(&self.metrics)?;
        Ok(page)
    }

    fn take_lazy_reader(&mut self, si: usize, cancel: &AtomicBool) -> Result<(LazyReader, Value)> {
        check_cancel(cancel)?;
        if let Some(cached) = self.lazy_reader.take() {
            if self.spec.slices[cached.spec_index].spec == self.spec.slices[si].spec {
                self.metrics.source_acquisitions += 1;
                self.metrics.lazy_source_cache_hits += 1;
                let before = cached.source.diagnostics();
                return Ok((cached, before));
            }
            // `cached` is dropped here, before another original-source open.
        }
        let reserved = self
            .spec_bytes
            .checked_add(self.metrics.geometry_bytes)
            .and_then(|n| n.checked_add(self.spec.budget.tile_bytes))
            .and_then(|n| {
                self.spec
                    .budget
                    .output_bytes
                    .checked_mul(3)
                    .and_then(|o| n.checked_add(o))
            })
            .and_then(|n| n.checked_add(crate::source::NATIVE_SOURCE_RETAINED_BYTES))
            .and_then(|n| n.checked_add(std::mem::size_of::<LazyReader>()))
            .ok_or_else(|| anyhow::anyhow!("lazy source reader reservation overflow"))?;
        ensure!(
            reserved <= self.spec.budget.working_bytes,
            "lazy source reader exceeds working budget"
        );
        self.metrics.peak_tracked_bytes = self.metrics.peak_tracked_bytes.max(reserved);
        let source = crate::io::open_source(
            self.spec.slices[si].spec.as_ref().expect("lazy source"),
            cancel,
        )?;
        self.metrics.source_acquisitions += 1;
        self.metrics.source_opens += 1;
        let checked = (|| -> Result<()> {
            ensure!(
                source.retained_memory_bound() <= crate::source::NATIVE_SOURCE_RETAINED_BYTES,
                "lazy source reader exceeds its reserved capacity"
            );
            // Eviction or a failed cached verification must not allow reopening
            // the identical declaration as a different source generation.
            for (prior, pin) in self.pins.iter().enumerate() {
                if self.spec.slices[prior].spec == self.spec.slices[si].spec {
                    if let Some(pin) = pin {
                        ensure!(
                            source.metadata().source_id == pin.source_id
                                && source.metadata().grid.identity() == pin.grid_id,
                            "lazy source version changed after an earlier slice"
                        );
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = checked {
            self.record_source_access(&json!({}), &source.diagnostics());
            return Err(error);
        }
        Ok((
            LazyReader {
                spec_index: si,
                source,
            },
            json!({}),
        ))
    }

    fn verify_resume_pin(
        &self,
        si: usize,
        source: &dyn WindowSource,
        pin: &Pin,
        cancel: &AtomicBool,
    ) -> Result<()> {
        check_cancel(cancel)?;
        source.verify_immutable()?;
        ensure!(
            source.metadata().source_id == pin.source_id
                && source.metadata().grid.identity() == pin.grid_id,
            "checkpoint source version changed"
        );
        if let Some(path) = &self.spec.slices[si].index {
            let ix = PersistedIndex::open(
                path,
                pin.index_build_id.as_deref(),
                self.spec.budget.tile_bytes,
                cancel,
            )?;
            ix.verify_source_metadata(source)?;
        }
        check_cancel(cancel)
    }

    fn record_source_access(&mut self, before: &Value, after: &Value) {
        let delta = |key: &str| {
            after["remote"][key]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(before["remote"][key].as_u64().unwrap_or(0))
        };
        self.metrics.network_requests += delta("requests");
        self.metrics.network_body_bytes += delta("received_bytes");
        self.metrics.network_get_requests += delta("get_requests");
        self.metrics.network_head_requests += delta("head_requests");
        self.metrics.transfer_cache_hits += delta("cache_hits");
    }

    fn run_slice(
        &mut self,
        si: usize,
        source: &dyn WindowSource,
        cancel: &AtomicBool,
        cache: &mut TileCache,
    ) -> Result<Vec<(Value, usize)>> {
        let verification_start = Instant::now();
        source.verify_immutable()?;
        let meta = source.metadata();
        meta.grid.validate()?;
        let cache_identity = crate::tile_cache::source_key(source)?;
        let cache_before = cache.stats();
        self.metrics.decoded_cache_budget_bytes = cache.limit();
        let mut index = if self.spec.schedule == Schedule::Mixed {
            self.spec.slices[si]
                .index
                .as_ref()
                .map(|path| {
                    PersistedIndex::open(
                        path,
                        self.spec.slices[si].expected_build_id.as_deref(),
                        self.spec.budget.tile_bytes,
                        cancel,
                    )
                })
                .transpose()?
        } else {
            None
        };
        if let Some(ix) = &index {
            ix.verify_source_metadata(source)?;
        }
        self.metrics.verification_ms += verification_start.elapsed().as_secs_f64() * 1000.;
        let hash_start = Instant::now();
        let grid_id = meta.grid.identity();
        self.metrics.grid_hash_ms += hash_start.elapsed().as_secs_f64() * 1000.;
        let pin = Pin {
            source_id: meta.source_id.clone(),
            grid_id,
            index_build_id: index.as_ref().map(|i| i.header().build_id.clone()),
        };
        if let Some(old) = &self.pins[si] {
            ensure!(
                old.source_id == pin.source_id
                    && old.grid_id == pin.grid_id
                    && old.index_build_id == pin.index_build_id,
                "batch source or index version changed"
            );
        }
        ensure!(
            pin_is_bounded(&pin),
            "batch pin identifier exceeds source or digest bounds"
        );
        self.pins[si] = Some(pin.clone());
        let stored = source.stored_summaries();
        if let Some(provider) = stored {
            provider.summary_layout().validate(&meta.grid)?;
        }
        let edge = index.as_ref().map_or_else(
            || stored.map_or(self.spec.tile_edge, |p| p.summary_layout().tile_edge),
            |i| i.header().tile_edge,
        );
        let mut key = format!("{}:{edge}", pin.grid_id);
        let mut options = self.spec.options.clone();
        if let Some(bands) = &self.spec.slices[si].bands {
            options.bands = bands.clone();
        }
        let output_bands = if self.spec.expression.is_some() {
            vec![0]
        } else {
            options.selected_bands(meta.bands.len())
        };
        if self.spec.expression.is_some() {
            ensure!(
                options.bands.is_empty() || options.bands == [0],
                "derived expression exposes one output band"
            );
            ensure!(
                options.weight_band.is_none(),
                "derived expressions currently require explicit separate weight-free reductions"
            );
            options.bands = vec![0];
            options.validate(1)?;
        } else {
            crate::stored_summary::selected_bands(&options, meta.bands.len())?;
        }
        let mut read_bands = BTreeSet::new();
        if let Some(e) = &self.spec.expression {
            read_bands.extend(e.validate(meta.bands.len())?);
        } else {
            read_bands.extend(output_bands.iter().copied());
        }
        if let Some(m) = &self.spec.mask {
            read_bands.extend(m.validate(meta.bands.len())?);
        }
        if let Some(w) = options.weight_band {
            read_bands.insert(w);
        }
        // Constants still require a native tile to establish shape; no validity inherited from this band.
        if read_bands.is_empty() {
            read_bands.insert(0);
        }
        let read_bands: Vec<_> = read_bands.into_iter().collect();
        ensure!(
            output_bands.len() <= 20
                || (self.spec.expression.is_none()
                    && self.spec.mask.is_none()
                    && options.weight_band.is_none()),
            "more than20 batch output bands require direct unweighted bands without expression or query mask"
        );
        if self.spec.window_policy == WindowPolicy::SourceLayout
            && index.is_none()
            && stored.is_none()
            && read_bands.len() <= source.max_read_bands().min(64)
        {
            // Same mathematical grid does not imply identical decoder/mask state.
            // Bounds for every allowed edge include custom-reader and mask layouts.
            // Band ordinal is not a physical fact: identical selected layouts and
            // bounds may reuse geometry across dates. Values/masks are read anew.
            let bounds: Vec<_> = [32usize, 64, 128, 256, 512]
                .into_iter()
                .map(|edge| {
                    let mut shapes = BTreeSet::new();
                    for w in [edge.min(meta.grid.width), meta.grid.width % edge] {
                        for h in [edge.min(meta.grid.height), meta.grid.height % edge] {
                            if w > 0 && h > 0 {
                                shapes.insert((w, h));
                            }
                        }
                    }
                    shapes
                        .into_iter()
                        .map(|(w, h)| (w, h, source.read_buffer_bound(w, h, &read_bands).ok()))
                        .collect::<Vec<_>>()
                })
                .collect();
            let blocks: Vec<_> = read_bands
                .iter()
                .map(|&bi| meta.bands[bi].block_size)
                .collect();
            key.push_str(
                &blake3::hash(&serde_json::to_vec(&(
                    source.access_layout(),
                    bounds,
                    blocks,
                    output_bands.len(),
                ))?)
                .to_hex(),
            );
        }
        let acc_bytes = Acc::retained_bytes(&options)?
            .checked_mul(output_bands.len())
            .and_then(|n| n.checked_mul(self.spec.zones.len()))
            .and_then(|n| n.checked_add(Acc::transient_bytes(&options)))
            .ok_or_else(|| anyhow::anyhow!("batch accumulator overflow"))?;
        let fixed = acc_bytes
            .saturating_add(self.spec.budget.tile_bytes)
            .saturating_add(self.spec.budget.output_bytes.saturating_mul(3))
            .saturating_add(self.spec_bytes)
            .saturating_add(self.checkpoint_reserve)
            .saturating_add(source.retained_memory_bound())
            .saturating_add(if self.spec.slices[si].spec.is_some() {
                std::mem::size_of::<LazyReader>()
            } else {
                0
            });
        ensure!(
            fixed <= self.spec.budget.working_bytes,
            "batch accumulator/source/output working budget exceeded before geometry compilation"
        );
        let geometry_limit = self
            .spec
            .budget
            .geometry_bytes
            .min(self.spec.budget.working_bytes - fixed);
        let used: usize = self.geometries.values().map(|g| g.bytes).sum();
        ensure!(
            used <= geometry_limit,
            "retained geometry exceeds remaining working budget"
        );
        if !self.geometries.contains_key(&key) {
            let start = Instant::now();
            let mut g =
                compile_geometry(&self.spec, &meta.grid, edge, geometry_limit - used, cancel)?;
            self.metrics.compilation_ms += start.elapsed().as_secs_f64() * 1000.;
            self.metrics.geometry_compilations += self.spec.zones.len();
            if self.spec.window_policy == WindowPolicy::SourceLayout
                && index.is_none()
                && stored.is_none()
                && read_bands.len() <= source.max_read_bands().min(64)
            {
                let policy_start = Instant::now();
                let planner = g.tiles.len().saturating_mul(192).saturating_add(4096);
                let remaining = geometry_limit.saturating_sub(used).saturating_sub(g.bytes);
                if planner <= remaining && g.tiles.len() <= 16384 {
                    let occupied: Vec<_> = g.tiles.keys().copied().collect();
                    let mut decision = crate::source::select_window_edge(
                        source,
                        edge,
                        &read_bands,
                        &occupied,
                        self.spec.budget.tile_bytes,
                    )?;
                    decision.selected_edge = decision
                        .candidates
                        .iter()
                        .rev()
                        .find(|candidate| {
                            let extra = candidate
                                .edge
                                .saturating_mul(candidate.edge)
                                .saturating_mul(output_bands.len())
                                .saturating_mul(9)
                                .saturating_add(
                                    RowSummary::estimate_bytes(candidate.edge, &options)
                                        .unwrap_or(0),
                                );
                            candidate.eligible
                                && candidate.read_buffer_bound_bytes.saturating_add(extra)
                                    <= self.spec.budget.tile_bytes
                                && candidate.windows <= self.spec.budget.max_windows
                                && candidate
                                    .decoded_cells
                                    .saturating_mul(read_bands.len() as u64)
                                    .saturating_mul(8)
                                    <= self.spec.budget.decoded_bytes
                        })
                        .map_or(edge, |candidate| candidate.edge);
                    self.metrics.last_window_policy = Some(serde_json::to_value(&decision)?);
                    self.metrics.peak_tracked_bytes = self.metrics.peak_tracked_bytes.max(
                        fixed
                            .saturating_add(used)
                            .saturating_add(g.bytes)
                            .saturating_add(planner),
                    );
                    self.metrics.window_policy_ms += policy_start.elapsed().as_secs_f64() * 1000.;
                    if decision.selected_edge != edge {
                        let compile_start = Instant::now();
                        // Both geometries coexist only inside an explicitly reduced
                        // budget. Optional rejection preserves the valid base plan.
                        match compile_geometry(
                            &self.spec,
                            &meta.grid,
                            decision.selected_edge,
                            remaining.saturating_sub(planner),
                            cancel,
                        ) {
                            Ok(replacement) => {
                                self.metrics.peak_tracked_bytes =
                                    self.metrics.peak_tracked_bytes.max(
                                        fixed
                                            .saturating_add(used)
                                            .saturating_add(g.bytes)
                                            .saturating_add(planner)
                                            .saturating_add(replacement.bytes),
                                    );
                                g = replacement;
                                self.metrics.window_policy_promotions += 1;
                                self.metrics.geometry_compilations += self.spec.zones.len();
                            }
                            Err(_) => {
                                check_cancel(cancel)?;
                                self.metrics.window_policy_fallbacks += 1;
                            }
                        }
                        self.metrics.compilation_ms +=
                            compile_start.elapsed().as_secs_f64() * 1000.;
                    }
                } else {
                    self.metrics.window_policy_fallbacks += 1;
                    self.metrics.window_policy_ms += policy_start.elapsed().as_secs_f64() * 1000.;
                }
            }
            if let Some(decision) = self.metrics.last_window_policy.as_mut() {
                decision["executed_edge"] = json!(g.edge);
                decision["replacement_accepted"] = json!(g.edge != edge);
            }
            let digest_start = Instant::now();
            let mut digest = blake3::Hasher::new();
            digest.update(self.metrics.geometry_layout_digest.as_bytes());
            digest.update(&g.edge.to_le_bytes());
            for ((ty, tx), tile) in &g.tiles {
                digest.update(&ty.to_le_bytes());
                digest.update(&tx.to_le_bytes());
                for selection in &tile.selections {
                    digest.update(&selection.zone.to_le_bytes());
                    for span in &selection.spans {
                        digest.update(&span.row.to_le_bytes());
                        digest.update(&span.start.to_le_bytes());
                        digest.update(&span.end.to_le_bytes());
                    }
                    for cell in &selection.cells {
                        digest.update(&cell.row.to_le_bytes());
                        digest.update(&cell.col.to_le_bytes());
                        digest.update(&cell.fraction.to_bits().to_le_bytes());
                    }
                }
            }
            self.metrics.geometry_layout_digest = digest.finalize().to_hex().to_string();
            self.metrics.geometry_tasks += g.tiles.len();
            self.metrics.compilation_ms += digest_start.elapsed().as_secs_f64() * 1000.;
            self.geometries.insert(key.clone(), g);
        } else {
            self.metrics.geometry_cache_hits += self.spec.zones.len();
        }
        self.metrics.geometry_bytes = self.geometries.values().map(|g| g.bytes).sum();
        let geometry = &self.geometries[&key];
        let tracked = fixed.saturating_add(self.metrics.geometry_bytes);
        ensure!(
            tracked <= self.spec.budget.working_bytes,
            "batch accumulator/geometry/output working budget exceeded"
        );
        self.metrics.accumulator_reserved_bytes = acc_bytes;
        self.metrics.peak_tracked_bytes = self.metrics.peak_tracked_bytes.max(tracked);
        let bins = options.histogram_edges.as_ref().map_or(0, |e| e.len() - 1);
        let merge_key: Arc<str> = Arc::from(options.reducer_identity());
        self.metrics.reducer_identity_builds += 1;
        self.metrics.accumulator_states += self.spec.zones.len() * output_bands.len();
        let mut accumulators: Vec<Vec<Acc>> = (0..self.spec.zones.len())
            .map(|_| {
                output_bands
                    .iter()
                    .map(|_| Acc::new_shared(bins, &options, Arc::clone(&merge_key)))
                    .collect()
            })
            .collect();
        let mut slice_windows = 0usize;
        let mut slice_decoded = 0u64;
        let mut contributions = 0usize;
        let eligible = options.summaries_eligible()
            && self.spec.expression.is_none()
            && self.spec.mask.is_none();
        // Feature baseline deliberately rereads each polygon's tiles; shared modes process a tile once.
        let passes = if self.spec.schedule == Schedule::Feature {
            self.spec.zones.len()
        } else {
            1
        };
        for pass in 0..passes {
            for (&(ty, tx), tile) in &geometry.tiles {
                check_cancel(cancel)?;
                let selected: Vec<_> = tile
                    .selections
                    .iter()
                    .filter(|s| self.spec.schedule != Schedule::Feature || s.zone == pass)
                    .collect();
                if selected.is_empty() {
                    continue;
                }
                let mut summarized = BTreeSet::new();
                if eligible {
                    if let Some(ix) = &mut index {
                        let whole: Vec<_> = selected.iter().filter(|s| s.whole).collect();
                        if !whole.is_empty() {
                            let summaries =
                                ix.read_leaf_summary(ty * ix.header().tiles_x() + tx, cancel)?;
                            self.metrics.summary_records_read += 1;
                            for s in whole {
                                for (oi, &bi) in output_bands.iter().enumerate() {
                                    work(
                                        &mut contributions,
                                        self.spec.budget.max_contributions,
                                        cancel,
                                    )?;
                                    let sum = &summaries[bi];
                                    accumulators[s.zone][oi].merge_summary(
                                        sum.sum,
                                        sum.valid_count,
                                        sum.min,
                                        sum.max,
                                    );
                                }
                                summarized.insert(s.zone);
                                self.metrics.summarized_polygon_tiles += 1;
                            }
                        }
                    } else if let Some(provider) = stored {
                        let whole: Vec<_> = selected.iter().filter(|s| s.whole).collect();
                        if !whole.is_empty() {
                            let record = ty * provider.summary_layout().levels[0].0 + tx;
                            for (group_number, group) in output_bands.chunks(20).enumerate() {
                                ensure!(
                                    provider.summary_read_buffer_bound(group)?
                                        <= self.spec.budget.tile_bytes,
                                    "batch source summary read exceeds tile budget"
                                );
                                let states = provider.read_summary(record, group, cancel)?;
                                ensure!(
                                    states.len() == group.len(),
                                    "source returned incompatible summary band count"
                                );
                                self.metrics.summary_records_read += 1;
                                self.metrics.source_summary_records_read += 1;
                                for selection in &whole {
                                    for (offset, state) in states.iter().enumerate() {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        accumulators[selection.zone][group_number * 20 + offset]
                                            .merge_summary(
                                                state.sum,
                                                state.valid_count,
                                                state.min,
                                                state.max,
                                            );
                                    }
                                }
                            }
                            for selection in whole {
                                summarized.insert(selection.zone);
                                self.metrics.summarized_polygon_tiles += 1;
                            }
                        }
                    }
                }
                if selected.iter().all(|s| summarized.contains(&s.zone)) {
                    continue;
                }
                let (x, y) = (tx * geometry.edge, ty * geometry.edge);
                let (width, height) = (
                    geometry.edge.min(geometry.grid.width - x),
                    geometry.edge.min(geometry.grid.height - y),
                );
                // Read width follows the source capability and actual live
                // buffers. Geometry, consumers and per-band reduction order
                // remain shared across the complete tile.
                let read_band_limit = source.max_read_bands();
                ensure!(
                    (20..=64).contains(&read_band_limit),
                    "invalid source read band capability"
                );
                let mut group_start = 0;
                while group_start < output_bands.len() {
                    let mut group_end = (group_start + read_band_limit).min(output_bands.len());
                    let row_bound = if eligible
                        && matches!(
                            self.spec.geometry_layout,
                            GeometryLayout::CompactRows | GeometryLayout::Auto
                        ) {
                        RowSummary::estimate_bytes(width, &options).unwrap_or(0)
                    } else {
                        0
                    };
                    // Direct bands borrow the source window and allocate no
                    // derived raster. Charge that buffer only when constructed.
                    let derived_per_band =
                        if self.spec.expression.is_some() || self.spec.mask.is_some() {
                            width.saturating_mul(height).saturating_mul(9)
                        } else {
                            0
                        };
                    if output_bands.len() > 20 {
                        while source
                            .read_buffer_bound(
                                width,
                                height,
                                &output_bands[group_start..group_end],
                            )?
                            .saturating_add(
                                derived_per_band.saturating_mul(group_end - group_start),
                            )
                            .saturating_add(row_bound)
                            > self.spec.budget.tile_bytes
                        {
                            ensure!(
                                group_end > group_start + 1,
                                "batch tile allocation budget exceeded even for one band"
                            );
                            group_end -= 1;
                        }
                    }
                    let output_group = &output_bands[group_start..group_end];
                    let group_read;
                    let read_bands = if output_bands.len() <= 20 {
                        &read_bands
                    } else {
                        group_read = output_group.to_vec();
                        &group_read
                    };
                    let input_bound = source.read_buffer_bound(width, height, &read_bands)?;
                    let derived_bound = derived_per_band.saturating_mul(output_group.len());
                    // Optional tile-local mask reuse never changes admission. If
                    // the extra byte mask/header cannot coexist with the same
                    // source and derived windows, retain the original path.
                    let mask_scratch = width
                        .saturating_mul(height)
                        .saturating_add(std::mem::size_of::<Vec<u8>>());
                    let shared_mask_bound = if self.spec.expression.is_none()
                        && self.spec.mask.is_some()
                        && output_group.len() > 1
                        && input_bound
                            .saturating_add(derived_bound)
                            .saturating_add(row_bound)
                            .saturating_add(mask_scratch)
                            <= self.spec.budget.tile_bytes
                    {
                        mask_scratch
                    } else {
                        0
                    };
                    ensure!(
                        input_bound
                            .saturating_add(derived_bound)
                            .saturating_add(row_bound)
                            .saturating_add(shared_mask_bound)
                            <= self.spec.budget.tile_bytes,
                        "batch tile allocation budget exceeded"
                    );
                    slice_windows += 1;
                    self.metrics.band_group_reads += 1;
                    slice_decoded = slice_decoded
                        .checked_add((width * height * read_bands.len() * 8) as u64)
                        .ok_or_else(|| anyhow::anyhow!("decoded byte count overflow"))?;
                    ensure!(
                        slice_windows <= self.spec.budget.max_windows
                            && slice_decoded <= self.spec.budget.decoded_bytes,
                        "batch slice IO budget exceeded"
                    );
                    let start = Instant::now();
                    let cache_started = Instant::now();
                    let cache_key = crate::tile_cache::window_key(
                        &cache_identity,
                        [x, y, width, height],
                        &read_bands,
                    );
                    let cached = cache.get(&cache_key);
                    self.metrics.decoded_cache_ms += cache_started.elapsed().as_secs_f64() * 1000.;
                    let raster = if let Some(raster) = cached {
                        self.metrics.decoded_cache_hits += 1;
                        raster
                    } else {
                        let (raster, read_metrics) = source.read_selected_window_cancellable(
                            x,
                            y,
                            width,
                            height,
                            &read_bands,
                            self.spec.budget.tile_bytes
                                - derived_bound
                                - row_bound
                                - shared_mask_bound,
                            cancel,
                        )?;
                        let validation_started = Instant::now();
                        crate::source::validate_window(
                            meta,
                            &raster,
                            [x, y, width, height],
                            &read_bands,
                        )?;
                        self.metrics.read_window_validation_ms +=
                            validation_started.elapsed().as_secs_f64() * 1000.;
                        self.metrics.read_decode_ms += read_metrics.read_decode_ms;
                        self.metrics.normalization_ms += read_metrics.normalization_ms;
                        self.metrics.raster_io_calls += read_metrics.raster_io_calls;
                        self.metrics.windows_read += 1;
                        self.metrics.decoded_value_bytes +=
                            (width * height * read_bands.len() * 8) as u64;
                        let raster = Arc::new(raster);
                        let cache_started = Instant::now();
                        cache.insert(cache_key, Arc::clone(&raster));
                        self.metrics.decoded_cache_ms +=
                            cache_started.elapsed().as_secs_f64() * 1000.;
                        raster
                    };
                    check_cancel(cancel)?;
                    self.metrics.io_ms += start.elapsed().as_secs_f64() * 1000.;
                    let expr_start = Instant::now();
                    let derived = if let Some(e) = &self.spec.expression {
                        Some(vec![e.evaluate(
                            self.spec.mask.as_ref(),
                            &raster,
                            &read_bands,
                            cancel,
                        )?])
                    } else if shared_mask_bound > 0 {
                        let mask = self.spec.mask.as_ref().unwrap().evaluate_mask(
                            &raster,
                            &read_bands,
                            cancel,
                        )?;
                        self.metrics.mask_expression_cells += mask.len() as u64;
                        self.metrics.mask_expression_passes += 1;
                        self.metrics.shared_mask_peak_bytes =
                            self.metrics.shared_mask_peak_bytes.max(shared_mask_bound);
                        let mut bands = Vec::with_capacity(output_bands.len());
                        for &band in &output_bands {
                            let input = &raster.bands
                                [read_bands.iter().position(|&index| index == band).unwrap()];
                            let mut values = Vec::with_capacity(mask.len());
                            let mut valid = Vec::with_capacity(mask.len());
                            for (i, &admitted) in mask.iter().enumerate() {
                                if i % 4096 == 0 {
                                    check_cancel(cancel)?;
                                }
                                let keep = admitted != 0 && input.valid[i];
                                values.push(if keep { input.values[i] } else { 0. });
                                valid.push(keep);
                            }
                            bands.push(Band {
                                values,
                                valid,
                                unit: None,
                            });
                        }
                        Some(bands)
                    } else if self.spec.mask.is_some() {
                        let bands = output_bands
                            .iter()
                            .map(|&band| {
                                Expr::Band { band }.evaluate(
                                    self.spec.mask.as_ref(),
                                    &raster,
                                    &read_bands,
                                    cancel,
                                )
                            })
                            .collect::<Result<Vec<_>>>()?;
                        self.metrics.mask_expression_cells +=
                            (raster.grid.cells()? as u64) * (output_bands.len() as u64);
                        self.metrics.mask_expression_passes += output_bands.len() as u64;
                        Some(bands)
                    } else {
                        None
                    };
                    self.metrics.expression_ms += expr_start.elapsed().as_secs_f64() * 1000.;
                    let weights = options
                        .weight_band
                        .map(|b| &raster.bands[read_bands.iter().position(|v| *v == b).unwrap()]);
                    let reduction_start = Instant::now();
                    if matches!(
                        self.spec.geometry_layout,
                        GeometryLayout::CompactRows | GeometryLayout::Auto
                    ) && eligible
                        && self.spec.schedule != Schedule::Feature
                    {
                        for (oi, &bi) in output_bands
                            .iter()
                            .enumerate()
                            .filter(|(_, bi)| output_group.contains(bi))
                        {
                            let band =
                                &raster.bands[read_bands.iter().position(|v| *v == bi).unwrap()];
                            for (&row, queries) in &tile.rows {
                                check_cancel(cancel)?;
                                if queries
                                    .iter()
                                    .all(|&(si, _)| summarized.contains(&tile.selections[si].zone))
                                {
                                    continue;
                                }
                                // Preflight the worst-case two input scans before any row allocation.
                                contributions = contributions
                                    .saturating_add(width * (1 + usize::from(options.needs_sum())));
                                ensure!(
                                    contributions <= self.spec.budget.max_contributions,
                                    "batch contribution budget exceeded"
                                );
                                let mut visits = 0;
                                let prefix = RowSummary::build_counted(
                                    band,
                                    row,
                                    width,
                                    &options,
                                    &mut visits,
                                );
                                self.metrics.raw_band_cell_visits += visits;
                                self.metrics.row_prefix_input_visits += visits;
                                self.metrics.row_prefix_builds += usize::from(prefix.is_some());
                                self.metrics.row_prefix_rejections += usize::from(prefix.is_none());
                                for &(si, qi) in queries {
                                    let selection = &tile.selections[si];
                                    if summarized.contains(&selection.zone) {
                                        continue;
                                    }
                                    let span = &selection.spans[qi];
                                    work(
                                        &mut contributions,
                                        self.spec.budget.max_contributions,
                                        cancel,
                                    )?;
                                    let acc = &mut accumulators[selection.zone][oi];
                                    if let Some((sum, count, min, max)) =
                                        prefix.as_ref().and_then(|p| p.range(span.start, span.end))
                                    {
                                        acc.merge_summary(sum, count, min, max);
                                        self.metrics.row_prefix_ranges += 1;
                                    } else {
                                        self.metrics.row_range_fallbacks += 1;
                                        for col in span.start..span.end {
                                            work(
                                                &mut contributions,
                                                self.spec.budget.max_contributions,
                                                cancel,
                                            )?;
                                            acc.cell(band, row * width + col, 1., None, None);
                                            self.metrics.raw_band_cell_visits += 1;
                                        }
                                    }
                                }
                            }
                            for selection in &selected {
                                if summarized.contains(&selection.zone) {
                                    continue;
                                }
                                // Auto leaves unprofitable rows in the ordinary compact path.
                                for span in &selection.spans {
                                    if tile.rows.contains_key(&span.row) {
                                        continue;
                                    }
                                    self.metrics.row_direct_spans += 1;
                                    for col in span.start..span.end {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        accumulators[selection.zone][oi].cell(
                                            band,
                                            span.row * width + col,
                                            1.,
                                            None,
                                            None,
                                        );
                                        self.metrics.raw_band_cell_visits += 1;
                                    }
                                }
                                for cell in &selection.cells {
                                    work(
                                        &mut contributions,
                                        self.spec.budget.max_contributions,
                                        cancel,
                                    )?;
                                    accumulators[selection.zone][oi].cell(
                                        band,
                                        cell.row * width + cell.col,
                                        cell.fraction,
                                        None,
                                        None,
                                    );
                                    self.metrics.raw_band_cell_visits += 1;
                                }
                            }
                        }
                    } else if self.spec.geometry_layout == GeometryLayout::CompactShared
                        && eligible
                        && self.spec.schedule != Schedule::Feature
                    {
                        for p in &tile.shared {
                            let consumers: Vec<_> = p
                                .zones
                                .iter()
                                .copied()
                                .filter(|z| !summarized.contains(z))
                                .collect();
                            if consumers.is_empty() {
                                continue;
                            }
                            for (oi, &bi) in output_bands
                                .iter()
                                .enumerate()
                                .filter(|(_, bi)| output_group.contains(bi))
                            {
                                let band = &raster.bands
                                    [read_bands.iter().position(|v| *v == bi).unwrap()];
                                if p.fraction == 1. {
                                    let mut sum = Sum::default();
                                    let mut count = 0;
                                    let mut min = f64::INFINITY;
                                    let mut max = f64::NEG_INFINITY;
                                    for col in p.start..p.end {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        let i = p.row * width + col;
                                        if band.valid[i] {
                                            if options.needs_sum() {
                                                sum.add(band.values[i]);
                                            }
                                            count += 1;
                                            if options.needs_min() {
                                                min = min.min(band.values[i]);
                                            }
                                            if options.needs_max() {
                                                max = max.max(band.values[i]);
                                            }
                                        }
                                    }
                                    for &z in &consumers {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        accumulators[z][oi].merge_summary(sum, count, min, max);
                                    }
                                    self.metrics.shared_range_reductions += 1;
                                } else {
                                    for &z in &consumers {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        accumulators[z][oi].cell(
                                            band,
                                            p.row * width + p.start,
                                            p.fraction,
                                            None,
                                            None,
                                        );
                                    }
                                }
                                self.metrics.raw_band_cell_visits += if p.fraction == 1. {
                                    (p.end - p.start) as u64
                                } else {
                                    consumers.len() as u64
                                };
                            }
                        }
                    } else {
                        for selection in &selected {
                            if summarized.contains(&selection.zone) {
                                continue;
                            }
                            for (oi, &bi) in output_bands
                                .iter()
                                .enumerate()
                                .filter(|(_, bi)| output_group.contains(bi))
                            {
                                let band = derived.as_ref().map_or_else(
                                    || {
                                        &raster.bands
                                            [read_bands.iter().position(|v| *v == bi).unwrap()]
                                    },
                                    |d| &d[oi],
                                );
                                let acc = &mut accumulators[selection.zone][oi];
                                for span in &selection.spans {
                                    for col in span.start..span.end {
                                        work(
                                            &mut contributions,
                                            self.spec.budget.max_contributions,
                                            cancel,
                                        )?;
                                        acc.cell(
                                            band,
                                            span.row * width + col,
                                            1.,
                                            weights,
                                            options.histogram_edges.as_deref(),
                                        );
                                        self.metrics.raw_band_cell_visits += 1;
                                    }
                                }
                                for cell in &selection.cells {
                                    work(
                                        &mut contributions,
                                        self.spec.budget.max_contributions,
                                        cancel,
                                    )?;
                                    acc.cell(
                                        band,
                                        cell.row * width + cell.col,
                                        cell.fraction,
                                        weights,
                                        options.histogram_edges.as_deref(),
                                    );
                                    self.metrics.raw_band_cell_visits += 1;
                                }
                            }
                        }
                    }
                    if options.has_new_reducers() {
                        for selection in &tile.selections {
                            if self.spec.schedule != Schedule::Feature || selection.zone == pass {
                                for acc in &accumulators[selection.zone] {
                                    acc.check_error()?;
                                }
                            }
                        }
                    }
                    self.metrics.reduction_ms += reduction_start.elapsed().as_secs_f64() * 1000.;
                    group_start = group_end;
                }
            }
        }
        let verification_start = Instant::now();
        source.verify_immutable()?;
        let cache_after = cache.stats();
        self.metrics.decoded_cache_hit_payload_bytes +=
            cache_after.hit_payload_bytes - cache_before.hit_payload_bytes;
        self.metrics.decoded_cache_admitted_payload_bytes +=
            cache_after.admitted_payload_bytes - cache_before.admitted_payload_bytes;
        self.metrics.decoded_cache_evictions += cache_after.evictions - cache_before.evictions;
        self.metrics.decoded_cache_admission_rejections +=
            cache_after.admission_rejections - cache_before.admission_rejections;
        self.metrics.decoded_cache_resident_bytes = cache.bytes();
        if let Some(ix) = &index {
            ix.verify()?;
        }
        self.metrics.verification_ms += verification_start.elapsed().as_secs_f64() * 1000.;
        let assembly_start = Instant::now();
        let assembly_accounting_before = self.metrics.output_accounting_ms;
        let mut rows = Vec::with_capacity(self.spec.zones.len());
        self.descriptor = if self.spec.output_mode == OutputMode::Numeric {
            Some(
                json!({"schema":"skarve_numeric_six_v1","mode":"native_grid_planar", "fingerprint":self.fingerprint,
                "source_id":pin.source_id,"grid_id":pin.grid_id,"slice_id":self.spec.slices[si].id,
                "time":self.spec.slices[si].time,"variable":self.spec.slices[si].variable,
                "statistics":options.statistics,"bands":output_bands.iter().map(|&bi| json!({"band":bi,"unit":if self.spec.expression.is_some(){None}else{meta.bands[bi].unit.clone()}})).collect::<Vec<_>>() }),
            )
        } else {
            None
        };
        self.descriptor_charge = self
            .descriptor
            .as_ref()
            .map(output_charge)
            .transpose()?
            .unwrap_or(0);
        let mut bytes = self.descriptor_charge;
        for (zi, accs) in accumulators.into_iter().enumerate() {
            check_cancel(cancel)?;
            let z = &self.spec.zones[zi];
            let measure = &geometry.zones[zi];
            let result_id = blake3::hash(
                format!("{}:{}:{}:{}", self.fingerprint, zi, si, pin.source_id).as_bytes(),
            )
            .to_hex()
            .to_string();
            let row = if self.spec.output_mode == OutputMode::Numeric {
                let numeric: Vec<NumericBand> = accs
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| a.finish_numeric(output_bands[i], cancel))
                    .collect::<Result<_>>()?;
                self.metrics.numeric_results_direct += numeric.len();
                json!({"result_id":result_id,"zone_id":z.id,"zone_version":z.version,"slice_id":self.spec.slices[si].id,"bands":numeric})
            } else {
                let bands = accs
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| {
                        a.finish_cancellable(
                            output_bands[i],
                            measure.selected,
                            measure.area,
                            measure.intersecting,
                            if self.spec.expression.is_some() {
                                None
                            } else {
                                meta.bands[output_bands[i]].unit.clone()
                            },
                            &options,
                            cancel,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut row = json!({"result_id":result_id,"zone_id":z.id,"zone_version":z.version,"slice_id":self.spec.slices[si].id,
                    "time":self.spec.slices[si].time,"variable":self.spec.slices[si].variable,"source_id":pin.source_id,"grid_id":pin.grid_id,
                    "mode":"native_grid_planar","bands":bands});
                options.project(&mut row);
                row
            };
            let accounting_start = Instant::now();
            let charge = output_charge(&row)?;
            bytes += charge;
            self.metrics.output_accounting_ms += accounting_start.elapsed().as_secs_f64() * 1000.;
            ensure!(
                bytes <= self.spec.budget.output_bytes,
                "batch output slice exceeds output budget"
            );
            rows.push((row, charge));
        }
        self.metrics.result_assembly_ms += assembly_start.elapsed().as_secs_f64() * 1000.
            - (self.metrics.output_accounting_ms - assembly_accounting_before);
        Ok(rows)
    }
}

fn retained_json(v: &Value) -> usize {
    let base = std::mem::size_of::<Value>();
    base.saturating_add(match v {
        Value::String(s) => s.capacity(),
        Value::Array(a) => a
            .capacity()
            .saturating_mul(base)
            .saturating_add(a.iter().map(retained_json).sum::<usize>()),
        Value::Object(o) => o
            .len()
            .saturating_mul(128)
            .saturating_add(1024)
            .saturating_add(
                o.iter()
                    .map(|(k, v)| k.capacity().saturating_add(retained_json(v)))
                    .sum::<usize>(),
            ),
        _ => 0,
    })
}
// Conservative JSON wire bound without rendering a row solely to measure it.
// A UTF-8 byte needs at most six bytes (\\u00XX); numbers at most32 bytes.
// This covers keys, delimiters, commas and both serialization/copy scratch.
fn json_wire_bound(v: &Value) -> usize {
    match v {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 32,
        Value::String(s) => s.len().saturating_mul(6).saturating_add(2),
        Value::Array(a) => a.iter().fold(2usize, |n, v| {
            n.saturating_add(1).saturating_add(json_wire_bound(v))
        }),
        Value::Object(o) => o.iter().fold(2usize, |n, (k, v)| {
            n.saturating_add(k.len().saturating_mul(6))
                .saturating_add(4)
                .saturating_add(json_wire_bound(v))
        }),
    }
}
fn output_charge(v: &Value) -> Result<usize> {
    Ok(retained_json(v)
        .saturating_add(json_wire_bound(v).saturating_mul(2))
        .saturating_add(256))
}

fn work(count: &mut usize, limit: usize, cancel: &AtomicBool) -> Result<()> {
    *count = count.saturating_add(1);
    ensure!(*count <= limit, "batch contribution budget exceeded");
    if *count % 4096 == 1 {
        check_cancel(cancel)?;
    }
    Ok(())
}

fn compile_geometry(
    spec: &JobSpec,
    grid: &Grid,
    edge: usize,
    budget: usize,
    cancel: &AtomicBool,
) -> Result<Geometry> {
    let mut tiles: BTreeMap<(usize, usize), Tile> = BTreeMap::new();
    let mut zones = Vec::new();
    let mut bytes = 4096;
    for (zi, zone) in spec.zones.iter().enumerate() {
        let plan = coverage::compile_with_budget(
            grid,
            &zone.geometry,
            &spec.crs,
            "scanline",
            cancel,
            budget.saturating_sub(bytes),
        )?;
        let temporary = plan.bytes();
        ensure!(
            bytes.saturating_add(temporary) <= budget,
            "batch geometry budget exceeded"
        );
        let mut selections: BTreeMap<(usize, usize), Selection> = BTreeMap::new();
        for s in &plan.spans {
            check_cancel(cancel)?;
            let mut col = s.start;
            while col < s.end {
                let key = (s.row / edge, col / edge);
                let end = s.end.min((key.1 + 1) * edge);
                if !selections.contains_key(&key) {
                    bytes += 1024;
                    ensure!(
                        bytes.saturating_add(temporary) <= budget,
                        "batch selection allocation budget exceeded"
                    );
                }
                let selection = selections.entry(key).or_insert_with(|| Selection {
                    zone: zi,
                    ..Default::default()
                });
                if spec.geometry_layout == GeometryLayout::Csr {
                    bytes = bytes.saturating_add((end - col).saturating_mul(48));
                    ensure!(
                        bytes.saturating_add(temporary) <= budget,
                        "batch CSR allocation budget exceeded"
                    );
                    for c in col..end {
                        selection.cells.push(Cell {
                            row: s.row % edge,
                            col: c % edge,
                            fraction: 1.,
                        });
                    }
                } else {
                    selection.spans.push(Span {
                        row: s.row % edge,
                        start: col % edge,
                        end: end - key.1 * edge,
                    });
                    bytes += 48;
                }
                ensure!(
                    bytes.saturating_add(temporary) <= budget,
                    "batch geometry budget exceeded"
                );
                col = end;
            }
        }
        for c in &plan.cells {
            let key = (c.row / edge, c.col / edge);
            if !selections.contains_key(&key) {
                bytes += 1024;
                ensure!(
                    bytes.saturating_add(temporary) <= budget,
                    "batch selection allocation budget exceeded"
                );
            }
            selections
                .entry(key)
                .or_insert_with(|| Selection {
                    zone: zi,
                    ..Default::default()
                })
                .cells
                .push(Cell {
                    row: c.row % edge,
                    col: c.col % edge,
                    fraction: c.fraction,
                });
            bytes += 48;
            ensure!(
                bytes.saturating_add(temporary) <= budget,
                "batch geometry budget exceeded"
            );
        }
        for (key, mut s) in selections {
            let full = edge.min(grid.width - key.1 * edge) * edge.min(grid.height - key.0 * edge);
            s.whole = s.cells.is_empty()
                && s.spans.iter().map(|s| s.end - s.start).sum::<usize>() == full;
            // CSR is an expanded representation of the same exact full-cell plan.
            if spec.geometry_layout == GeometryLayout::Csr {
                s.whole = s.cells.len() == full && s.cells.iter().all(|c| c.fraction == 1.);
            }
            bytes += 512;
            ensure!(
                bytes.saturating_add(temporary) <= budget,
                "batch geometry budget exceeded"
            );
            tiles.entry(key).or_default().selections.push(s);
        }
        zones.push(ZoneMeasure {
            selected: plan.selected,
            area: plan.polygon_area,
            intersecting: plan.intersecting,
        });
    }
    if spec.geometry_layout == GeometryLayout::CompactRows
        || (spec.geometry_layout == GeometryLayout::Auto
            && spec.options.summaries_eligible()
            && spec.expression.is_none()
            && spec.mask.is_none()
            && spec.schedule != Schedule::Feature)
    {
        for (&(_, tx), tile) in &mut tiles {
            check_cancel(cancel)?;
            let row_width = edge.min(grid.width - tx * edge);
            let scratch = edge.saturating_mul(24);
            if bytes.saturating_add(scratch) > budget
                && spec.geometry_layout == GeometryLayout::Auto
            {
                continue;
            }
            ensure!(
                bytes.saturating_add(scratch) <= budget,
                "row cost scratch budget exceeded"
            );
            let mut costs = vec![(0usize, 0usize); edge];
            for selection in &tile.selections {
                for span in &selection.spans {
                    costs[span.row].0 = costs[span.row].0.saturating_add(span.end - span.start);
                    costs[span.row].1 += 1;
                }
            }
            let mut accepted = vec![false; edge];
            for (row, &(visits, count)) in costs.iter().enumerate() {
                if count == 0
                    || (spec.geometry_layout == GeometryLayout::Auto
                        && visits
                            <= row_width
                                .saturating_mul(8)
                                .saturating_add(count.saturating_mul(8)))
                {
                    continue;
                }
                let charge = count.saturating_mul(32).saturating_add(128);
                if bytes.saturating_add(scratch).saturating_add(charge) > budget
                    && spec.geometry_layout == GeometryLayout::Auto
                {
                    continue;
                }
                ensure!(
                    bytes.saturating_add(scratch).saturating_add(charge) <= budget,
                    "row incidence allocation budget exceeded"
                );
                bytes = bytes.saturating_add(charge);
                accepted[row] = true;
            }
            // All incidence for a row is accepted atomically; partial incidence
            // would omit unlisted spans when the reducer selects the row helper.
            for (si, selection) in tile.selections.iter().enumerate() {
                for (qi, span) in selection.spans.iter().enumerate() {
                    if accepted[span.row] {
                        tile.rows.entry(span.row).or_default().push((si, qi));
                    }
                }
            }
        }
    }

    if spec.geometry_layout == GeometryLayout::CompactShared {
        for tile in tiles.values_mut() {
            check_cancel(cancel)?;
            let scratch = tile
                .selections
                .iter()
                .map(|s| {
                    s.spans
                        .len()
                        .saturating_mul(224)
                        .saturating_add(s.cells.len().saturating_mul(192))
                })
                .sum::<usize>()
                .saturating_add(spec.zones.len() * 128 + 4096);
            ensure!(
                bytes.saturating_add(scratch) <= budget,
                "shared incidence scratch budget exceeded"
            );
            // Split row spans at endpoints. Each value run has one exact set of consumers.
            let mut rows: BTreeMap<usize, Vec<(usize, usize, bool)>> = BTreeMap::new();
            let mut cells: BTreeMap<(usize, usize, u64), Vec<usize>> = BTreeMap::new();
            for s in &tile.selections {
                for span in &s.spans {
                    rows.entry(span.row)
                        .or_default()
                        .extend([(span.start, s.zone, true), (span.end, s.zone, false)]);
                }
                for c in &s.cells {
                    cells
                        .entry((c.row, c.col, c.fraction.to_bits()))
                        .or_default()
                        .push(s.zone);
                }
            }
            for (row, mut events) in rows {
                events.sort_unstable();
                let mut active = BTreeSet::new();
                let mut prev = 0;
                let mut i = 0;
                while i < events.len() {
                    let x = events[i].0;
                    if x > prev && !active.is_empty() {
                        bytes += 128 + active.len() * 16;
                        ensure!(
                            bytes.saturating_add(scratch) <= budget,
                            "shared incidence budget exceeded"
                        );
                        tile.shared.push(Piece {
                            row,
                            start: prev,
                            end: x,
                            fraction: 1.,
                            zones: active.iter().copied().collect(),
                        });
                    }
                    // Handle all ends and starts together, so touching intervals do not overlap.
                    while i < events.len() && events[i].0 == x {
                        let (_, z, start) = events[i];
                        if start {
                            active.insert(z);
                        } else {
                            active.remove(&z);
                        }
                        i += 1;
                    }
                    prev = x;
                }
            }
            for ((row, col, f), zones) in cells {
                bytes += 128 + zones.len() * 16;
                ensure!(
                    bytes.saturating_add(scratch) <= budget,
                    "shared incidence budget exceeded"
                );
                tile.shared.push(Piece {
                    row,
                    start: col,
                    end: col + 1,
                    fraction: f64::from_bits(f),
                    zones,
                });
            }
        }
    }
    ensure!(bytes <= budget, "batch geometry budget exceeded");
    Ok(Geometry {
        grid: grid.clone(),
        edge,
        zones,
        tiles,
        bytes,
    })
}

/// Zero-copy source ownership with bounded copied windows; useful for resident arrays.
pub struct ResidentSource<'a> {
    raster: &'a Raster,
    metadata: RasterMetadata,
}
impl<'a> ResidentSource<'a> {
    pub fn new(r: &'a Raster) -> Self {
        Self {
            raster: r,
            metadata: RasterMetadata {
                grid: r.grid.clone(),
                source_id: r.source_id.clone(),
                bands: r
                    .bands
                    .iter()
                    .map(|b| BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: b.unit.clone(),
                        block_size: (256, 256),
                    })
                    .collect(),
            },
        }
    }
}
impl WindowSource for ResidentSource<'_> {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        ensure!(
            width > 0
                && height > 0
                && x.checked_add(width)
                    .is_some_and(|v| v <= self.raster.grid.width)
                && y.checked_add(height)
                    .is_some_and(|v| v <= self.raster.grid.height),
            "resident window outside grid"
        );
        ensure!(
            self.read_buffer_bound(width, height, indices)? <= max_bytes
                && indices.iter().all(|b| *b < self.raster.bands.len()),
            "resident window budget or band invalid"
        );
        let mut grid = self.raster.grid.clone();
        grid.width = width;
        grid.height = height;
        grid.transform[0] += x as f64 * grid.transform[1];
        grid.transform[3] += y as f64 * grid.transform[5];
        let mut bands = Vec::new();
        for &b in indices {
            let input = &self.raster.bands[b];
            let mut values = Vec::with_capacity(width * height);
            let mut valid = Vec::with_capacity(width * height);
            for row in y..y + height {
                check_cancel(cancel)?;
                let from = row * self.raster.grid.width + x;
                values.extend_from_slice(&input.values[from..from + width]);
                valid.extend_from_slice(&input.valid[from..from + width]);
            }
            bands.push(Band {
                values,
                valid,
                unit: input.unit.clone(),
            });
        }
        Ok((
            Raster {
                grid,
                bands,
                source_id: self.raster.source_id.clone(),
            },
            ReadMetrics::default(),
        ))
    }
}
pub struct BorrowedSource<'a>(pub &'a dyn WindowSource);
impl WindowSource for BorrowedSource<'_> {
    fn boundary_prefetch_enabled(&self) -> bool {
        self.0.boundary_prefetch_enabled()
    }
    fn prefetch_boundary_windows(
        &self,
        windows: &[[usize; 4]],
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.0
            .prefetch_boundary_windows(windows, bands, max_bytes, cancel)
    }
    fn max_read_bands(&self) -> usize {
        self.0.max_read_bands()
    }
    fn stored_summaries(&self) -> Option<&dyn crate::stored_summary::StoredSummarySource> {
        self.0.stored_summaries()
    }
    fn metadata(&self) -> &RasterMetadata {
        self.0.metadata()
    }
    fn raw_metadata(&self) -> Option<&crate::source::RawRasterMetadata> {
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
    ) -> Result<(crate::source::RawWindow, ReadMetrics)> {
        self.0
            .read_raw_selected_window_cancellable(x, y, w, h, b, m, c)
    }
    fn verify_immutable(&self) -> Result<()> {
        self.0.verify_immutable()
    }
    fn diagnostics(&self) -> Value {
        self.0.diagnostics()
    }
    fn identity_descriptor(&self) -> Option<Value> {
        self.0.identity_descriptor()
    }
    fn access_layout(&self) -> Value {
        self.0.access_layout()
    }
    fn retained_memory_bound(&self) -> usize {
        self.0.retained_memory_bound()
    }
    fn read_buffer_bound(&self, w: usize, h: usize, b: &[usize]) -> Result<usize> {
        self.0.read_buffer_bound(w, h, b)
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        b: &[usize],
        m: usize,
        c: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        self.0.read_selected_window_cancellable(x, y, w, h, b, m, c)
    }
}

#[cfg(test)]
mod output_accounting_tests {
    use super::*;
    #[test]
    fn structural_wire_bound_covers_json_escaping_and_extreme_numbers() {
        let strings = ["plain", "\0\n\r\t\u{0008}\u{001f}\\\"", "é🗺️", ""];
        let numbers = [
            0.,
            -0.,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            -1e-100,
        ];
        for name in strings {
            for number in numbers {
                let row = json!({name: [number, u64::MAX, i64::MIN, null, true, false, {"inner":name.repeat(1024)}]});
                let actual = serde_json::to_vec(&row).unwrap().len();
                assert!(json_wire_bound(&row) >= actual);
                assert!(output_charge(&row).unwrap() >= retained_json(&row) + 2 * actual + 256);
            }
        }
    }
}
