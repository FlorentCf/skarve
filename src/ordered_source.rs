//! Explicit ordered selections over a source's raw scalar view. This does not
//! generate a polygon mask or change the ordinary fractional numerical policy.
use crate::{
    model::check_cancel,
    source::{RawScalarType, WindowSource},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, mem::size_of, sync::atomic::AtomicBool, time::Instant};

pub const POLICY: &str = "hm_demographics_ordered_v1";
const MAX_POLYGONS: usize = 256;
const MAX_WINDOWS: usize = 4096;
const MAX_BANDS: usize = 64;
const MAX_CONTRIBUTIONS: u64 = 268_435_456;
const MAX_READS: usize = 65_536;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrderedBudget {
    /// Includes owned selection/control, plan, partials, result and read scratch.
    /// The caller separately reserves the source's retained-memory bound.
    pub working_bytes: usize,
    pub planning_bytes: usize,
    pub max_read_calls: usize,
    pub max_contributions: u64,
    /// Cumulative returned typed samples plus mask bytes, not a claim about
    /// GDAL's internal decode traffic or physical HTTP transfer.
    pub read_materialized_bytes: u64,
}
impl Default for OrderedBudget {
    fn default() -> Self {
        Self {
            working_bytes: 64 << 20,
            planning_bytes: 16 << 20,
            max_read_calls: MAX_READS,
            max_contributions: MAX_CONTRIBUTIONS,
            read_materialized_bytes: 512 << 20,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedWindow {
    /// Pixel coordinates [x, y, width, height] in this source view.
    pub window: [usize; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<u32>>,
    /// Half-open local linear-index runs, strictly ordered and nonoverlapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs: Option<Vec<[u32; 2]>>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedPolygon {
    pub id: String,
    /// Logical window order is part of the numerical contract.
    pub windows: Vec<OrderedWindow>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedRequest {
    pub polygons: Vec<OrderedPolygon>,
    /// Empty means every exposed source band, in its exposed order.
    #[serde(default)]
    pub bands: Vec<usize>,
    /// Omission uses source metadata. A supplied null entry means no NoData.
    #[serde(default)]
    pub nodata: Option<Vec<Option<f64>>>,
    #[serde(default)]
    pub budget: OrderedBudget,
}
#[derive(Clone, Copy, Default)]
struct State {
    sum: f64,
    valid_count: u64,
    excluded_nodata: u64,
    excluded_nonfinite: u64,
    excluded_negative: u64,
}
impl State {
    fn add(&mut self, value: f64, nodata: Option<f64>) {
        // Same HM filtering as the existing supplied-selection bridge after its
        // documented nonfinite-NoData adaptation. Raw GDAL masks are ignored.
        if !value.is_finite() {
            self.excluded_nonfinite += 1;
        } else if nodata.is_some_and(|missing| value == missing) {
            self.excluded_nodata += 1;
        } else if value < 0. {
            self.excluded_negative += 1;
        } else {
            self.valid_count += 1;
            self.sum += value;
        }
    }
    fn merge_window(&mut self, other: Self) -> Result<()> {
        ensure!(other.sum.is_finite(), "ordered logical window sum overflow");
        self.sum += other.sum;
        ensure!(self.sum.is_finite(), "ordered polygon sum overflow");
        self.valid_count += other.valid_count;
        self.excluded_nodata += other.excluded_nodata;
        self.excluded_nonfinite += other.excluded_nonfinite;
        self.excluded_negative += other.excluded_negative;
        Ok(())
    }
}
fn checked_add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .context("ordered source allocation overflow")
}
fn checked_mul(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .context("ordered source allocation overflow")
}
fn input_bytes(request: &OrderedRequest) -> Result<usize> {
    let mut bytes = checked_add(
        size_of::<OrderedRequest>(),
        checked_mul(request.polygons.capacity(), size_of::<OrderedPolygon>())?,
    )?;
    bytes = checked_add(
        bytes,
        checked_mul(request.bands.capacity(), size_of::<usize>())?,
    )?;
    if let Some(nodata) = &request.nodata {
        bytes = checked_add(
            bytes,
            checked_mul(nodata.capacity(), size_of::<Option<f64>>())?,
        )?;
    }
    for polygon in &request.polygons {
        bytes = checked_add(bytes, polygon.id.capacity())?;
        bytes = checked_add(
            bytes,
            checked_mul(polygon.windows.capacity(), size_of::<OrderedWindow>())?,
        )?;
        for window in &polygon.windows {
            if let Some(indexes) = &window.indexes {
                bytes = checked_add(bytes, checked_mul(indexes.capacity(), 4)?)?;
            }
            if let Some(runs) = &window.runs {
                bytes = checked_add(bytes, checked_mul(runs.capacity(), 8)?)?;
            }
        }
    }
    Ok(bytes)
}
struct Admission {
    planning: usize,
    windows: usize,
    bands: usize,
    contributions: u64,
    control: usize,
}
fn validate_budget(b: &OrderedBudget) -> Result<()> {
    ensure!(
        (1 << 20..=256 << 20).contains(&b.working_bytes)
            && (1 << 16..=64 << 20).contains(&b.planning_bytes)
            && b.planning_bytes < b.working_bytes
            && (1..=MAX_READS).contains(&b.max_read_calls)
            && (1..=MAX_CONTRIBUTIONS).contains(&b.max_contributions)
            && (1..=2u64 << 30).contains(&b.read_materialized_bytes),
        "invalid ordered source budget"
    );
    Ok(())
}
fn admit(request: &OrderedRequest, bands: usize, cancel: &AtomicBool) -> Result<Admission> {
    check_cancel(cancel)?;
    let b = &request.budget;
    validate_budget(b)?;
    ensure!(
        !request.polygons.is_empty() && request.polygons.len() <= MAX_POLYGONS,
        "ordered polygon count exceeds bound"
    );
    ensure!(
        (1..=MAX_BANDS).contains(&bands),
        "ordered source requires1..64 bands"
    );
    ensure!(
        request
            .bands
            .iter()
            .enumerate()
            .all(|(i, v)| !request.bands[..i].contains(v)),
        "duplicate ordered source band"
    );
    if let Some(nodata) = &request.nodata {
        ensure!(
            nodata.len() == bands && nodata.iter().flatten().all(|v| v.is_finite()),
            "ordered NoData overrides require one finite-or-null value per selected band"
        );
    }
    let control = input_bytes(request)?;
    ensure!(
        control <= b.planning_bytes,
        "ordered source control exceeds planning budget"
    );
    let mut windows = 0usize;
    let mut selected = 0u64;
    for (p, polygon) in request.polygons.iter().enumerate() {
        check_cancel(cancel)?;
        ensure!(
            !polygon.id.is_empty()
                && polygon.id.len() <= 128
                && request.polygons[..p].iter().all(|old| old.id != polygon.id),
            "invalid or duplicate ordered polygon id"
        );
        windows = checked_add(windows, polygon.windows.len())?;
        ensure!(
            windows <= MAX_WINDOWS,
            "ordered logical window count exceeds bound"
        );
        for window in &polygon.windows {
            let [_, _, w, h] = window.window;
            let cells = checked_mul(w, h)?;
            ensure!(
                w > 0 && h > 0 && cells <= u32::MAX as usize,
                "invalid ordered logical window shape"
            );
            ensure!(
                window.indexes.is_some() != window.runs.is_some(),
                "provide exactly one ordered indexes or runs selection"
            );
            if let Some(indexes) = &window.indexes {
                let mut previous = None;
                for (i, &index) in indexes.iter().enumerate() {
                    if i % 1024 == 0 {
                        check_cancel(cancel)?;
                    }
                    ensure!(
                        (index as usize) < cells && previous.is_none_or(|p| p < index),
                        "ordered indexes must be strictly ascending and in bounds"
                    );
                    previous = Some(index);
                }
                selected = selected
                    .checked_add(indexes.len() as u64)
                    .context("ordered selection work overflow")?;
            } else if let Some(runs) = &window.runs {
                let mut previous = 0;
                for (i, &[start, end]) in runs.iter().enumerate() {
                    if i % 1024 == 0 {
                        check_cancel(cancel)?;
                    }
                    ensure!(
                        start < end && end as usize <= cells && (i == 0 || start >= previous),
                        "ordered runs must be nonempty, sorted, disjoint and in bounds"
                    );
                    previous = end;
                    selected = selected
                        .checked_add((end - start) as u64)
                        .context("ordered selection work overflow")?;
                }
            }
            ensure!(
                selected <= b.max_contributions,
                "ordered selection work exceeds contribution budget"
            );
        }
    }
    let contributions = selected
        .checked_mul(bands as u64)
        .context("ordered contribution overflow")?;
    ensure!(
        contributions <= b.max_contributions,
        "ordered selection work exceeds contribution budget"
    );
    // Conservative BTreeSet/node/vector ownership, partial states, per-group
    // cursors and final JSON allocation. No expanded per-cell run representation.
    let partials = checked_mul(checked_mul(windows, bands)?, size_of::<State>())?;
    let plan = checked_mul(windows, 512)?;
    let output = checked_add(
        checked_mul(checked_mul(request.polygons.len(), bands)?, 2048)?,
        2 << 20,
    )?;
    let planning = checked_add(control, checked_add(partials, checked_add(plan, output)?)?)?;
    ensure!(
        planning <= b.planning_bytes && planning < b.working_bytes,
        "ordered source plan/result exceeds planning budget"
    );
    Ok(Admission {
        planning,
        windows,
        bands,
        contributions,
        control,
    })
}
/// Constant-time session reservation; the source is separately retained.
/// Execute admits the source-resolved selection once before allocating its plan.
pub fn reservation_bytes(request: &OrderedRequest) -> Result<usize> {
    validate_budget(&request.budget)?;
    Ok(request.budget.working_bytes)
}
struct Cursor<'a> {
    window: &'a OrderedWindow,
    entry: usize,
    offset: u32,
}
impl<'a> Cursor<'a> {
    fn new(window: &'a OrderedWindow) -> Self {
        Self {
            window,
            entry: 0,
            offset: 0,
        }
    }
    fn peek(&self) -> Option<usize> {
        if let Some(indexes) = &self.window.indexes {
            indexes.get(self.entry).map(|v| *v as usize)
        } else {
            self.window
                .runs
                .as_ref()
                .and_then(|runs| runs.get(self.entry))
                .map(|r| (r[0] + self.offset) as usize)
        }
    }
    fn advance(&mut self) {
        if self.window.indexes.is_some() {
            self.entry += 1;
        } else {
            let runs = self.window.runs.as_ref().expect("validated selection");
            self.offset += 1;
            if runs[self.entry][0] + self.offset == runs[self.entry][1] {
                self.entry += 1;
                self.offset = 0;
            }
        }
    }
    fn point(&self) -> Option<(usize, usize)> {
        self.peek().map(|index| {
            let [x, y, w, _] = self.window.window;
            (y + index / w, x + index % w)
        })
    }
    fn last_row(&self) -> Option<usize> {
        let last = if let Some(indexes) = &self.window.indexes {
            indexes.last().copied()
        } else {
            self.window.runs.as_ref()?.last().map(|run| run[1] - 1)
        }? as usize;
        Some(self.window.window[1] + last / self.window.window[2])
    }
}
#[derive(Default, Serialize)]
struct Metrics {
    logical_windows: usize,
    unique_windows: usize,
    shared_window_references: usize,
    selected_contributions: u64,
    source_read_calls: usize,
    source_raster_io_calls: usize,
    raw_sample_bytes: u64,
    raw_mask_bytes: u64,
    read_materialized_bytes: u64,
    read_materialized_byte_limit: u64,
    read_bound_peak_bytes: usize,
    working_reserved_bytes: usize,
    planning_bound_bytes: usize,
    control_owned_bytes: usize,
    source_read_decode_ms: f64,
    reduction_ms: f64,
    elapsed_ms: f64,
}
/// Read shared source-row envelopes once per bounded band group/row stripe.
/// Physical stripe boundaries never reset a logical-window accumulator.
pub fn execute(
    source: &dyn WindowSource,
    request: &OrderedRequest,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    check_cancel(cancel)?;
    let raw_meta = source
        .raw_metadata()
        .context("ordered source requires original typed reads")?;
    let count = if request.bands.is_empty() {
        raw_meta.bands.len()
    } else {
        request.bands.len()
    };
    let admission = admit(request, count, cancel)?;
    let bands = if request.bands.is_empty() {
        (0..raw_meta.bands.len()).collect::<Vec<_>>()
    } else {
        request.bands.clone()
    };
    ensure!(
        bands.iter().all(|&b| b < raw_meta.bands.len()),
        "ordered source band out of bounds"
    );
    ensure!(
        bands.iter().all(|&b| matches!(
            raw_meta.bands[b].scalar_type,
            RawScalarType::Float32 | RawScalarType::Float64
        )),
        "ordered source policy requires original Float32 or Float64 bands"
    );
    let nodata = request.nodata.clone().unwrap_or_else(|| {
        bands
            .iter()
            .map(|&b| raw_meta.bands[b].nodata_f64_bits.map(f64::from_bits))
            .collect()
    });
    let grid = &source.metadata().grid;
    let mut groups = BTreeSet::new();
    let mut windows = Vec::with_capacity(admission.windows);
    let mut polygon_offsets = Vec::with_capacity(request.polygons.len() + 1);
    for polygon in &request.polygons {
        polygon_offsets.push(windows.len());
        for window in &polygon.windows {
            let [x, y, w, h] = window.window;
            ensure!(
                x.checked_add(w).is_some_and(|end| end <= grid.width)
                    && y.checked_add(h).is_some_and(|end| end <= grid.height),
                "ordered logical window outside source view"
            );
            groups.insert(window.window);
            windows.push(window);
        }
    }
    polygon_offsets.push(windows.len());
    let mut partials = vec![State::default(); admission.windows * admission.bands];
    let mut metrics = Metrics {
        logical_windows: admission.windows,
        unique_windows: groups.len(),
        shared_window_references: admission.windows - groups.len(),
        selected_contributions: admission.contributions,
        working_reserved_bytes: request.budget.working_bytes,
        planning_bound_bytes: admission.planning,
        control_owned_bytes: admission.control,
        read_materialized_byte_limit: request.budget.read_materialized_bytes,
        ..Default::default()
    };
    let read_bytes = request.budget.working_bytes - admission.planning;
    source.verify_immutable()?;
    // A one-cell admission maximizes band count at the cost of repeatedly
    // decoding the same physical blocks in one-row stripes. Admit against a
    // useful bounded spatial envelope first. This changes physical grouping
    // only: each logical window still has one uninterrupted per-band fold.
    let mut envelope = [grid.width, grid.height, 0, 0];
    for window in &windows {
        let cursor = Cursor::new(window);
        if let Some((row, _)) = cursor.point() {
            envelope[0] = envelope[0].min(window.window[0]);
            envelope[1] = envelope[1].min(row);
            envelope[2] = envelope[2].max(window.window[0] + window.window[2]);
            envelope[3] = envelope[3].max(cursor.last_row().expect("nonempty selection") + 1);
        }
    }
    let mut first = 0;
    while first < bands.len() && admission.contributions > 0 {
        check_cancel(cancel)?;
        let mut end = (first + source.max_read_bands().min(MAX_BANDS)).min(bands.len());
        ensure!(
            end > first,
            "ordered source has no admitted band read width"
        );
        while source.raw_read_buffer_bound(1, 1, &bands[first..end])? > read_bytes {
            ensure!(
                end > first + 1,
                "ordered source minimum read exceeds working budget"
            );
            end -= 1;
        }
        let probe_width = (envelope[2] - envelope[0]).min(65_536);
        let probe_height = (envelope[3] - envelope[1])
            .min(256)
            .min((65_536 / probe_width).max(1));
        // Prefer useful selected-band cells per bounded read. Maximizing only
        // area can choose one band of a pixel-interleaved source and repeatedly
        // decode all physical bands. Maximizing only bands can force one-row
        // reads of band-interleaved sources. Probe the finite band choices using
        // the reader's complete physical allocation bound, without source I/O.
        let mut chosen = first + 1;
        let mut best = 0;
        for candidate in first + 1..=end {
            let (mut pw, mut ph) = (probe_width, probe_height);
            while source.raw_read_buffer_bound(pw, ph, &bands[first..candidate])? > read_bytes {
                check_cancel(cancel)?;
                if ph > 1 {
                    ph = ph.div_ceil(2);
                } else {
                    ensure!(pw > 1, "ordered source minimum read exceeds working budget");
                    pw = pw.div_ceil(2);
                }
            }
            let score = pw * ph * (candidate - first);
            if score >= best {
                best = score;
                chosen = candidate;
            }
        }
        end = chosen;
        let mut cursors = windows
            .iter()
            .map(|window| Cursor::new(window))
            .collect::<Vec<_>>();
        while let Some((row, column)) = cursors.iter().filter_map(Cursor::point).min() {
            check_cancel(cancel)?;
            let mut ch = 256.min(grid.height - row);
            let (cx, cw, bound) = loop {
                // Include every selected cursor entering this row stripe.
                // Whole logical x extents conservatively enclose later runs
                // within the stripe without expanding those runs to cells.
                let mut left = grid.width;
                let mut right = 0;
                let mut last_row = row;
                for cursor in &cursors {
                    if cursor.point().is_some_and(|(y, _)| y < row + ch) {
                        let [x, _, w, _] = cursor.window.window;
                        left = left.min(x);
                        right = right.max(x + w);
                        last_row = last_row.max(cursor.last_row().expect("nonempty selection"));
                    }
                }
                ensure!(left < right, "empty ordered source stripe");
                // No active selection can consume rows after this exact tail.
                // Recompute the envelope after shortening the stripe.
                let selected_rows = last_row + 1 - row;
                if ch > selected_rows {
                    ch = selected_rows;
                    continue;
                }
                let cx = if ch == 1 { column } else { left };
                let mut cw = right - cx;
                let rows = (65_536 / cw).clamp(1, 256);
                if ch > rows {
                    ch = rows;
                    continue;
                }
                let bound = source.raw_read_buffer_bound(cw, ch, &bands[first..end])?;
                if bound <= read_bytes {
                    break (cx, cw, bound);
                }
                if ch > 1 {
                    ch = ch.div_ceil(2);
                    continue;
                }
                loop {
                    ensure!(cw > 1, "ordered source minimum read exceeds working budget");
                    cw = cw.div_ceil(2);
                    let bound = source.raw_read_buffer_bound(cw, 1, &bands[first..end])?;
                    if bound <= read_bytes {
                        break;
                    }
                }
                break (
                    cx,
                    cw,
                    source.raw_read_buffer_bound(cw, 1, &bands[first..end])?,
                );
            };
            ensure!(
                metrics.source_read_calls < request.budget.max_read_calls,
                "ordered source read-call budget exhausted"
            );
            let bytes_per_cell = bands[first..end]
                .iter()
                .map(|&b| raw_meta.bands[b].scalar_type.byte_width() + 1)
                .sum::<usize>();
            let materialized = (checked_mul(checked_mul(cw, ch)?, bytes_per_cell)?) as u64;
            ensure!(
                metrics
                    .read_materialized_bytes
                    .checked_add(materialized)
                    .is_some_and(|total| total <= request.budget.read_materialized_bytes),
                "ordered source materialized read-byte budget exhausted"
            );
            let (raw, read) = source.read_raw_selected_window_cancellable(
                cx,
                row,
                cw,
                ch,
                &bands[first..end],
                read_bytes,
                cancel,
            )?;
            metrics.source_read_calls += 1;
            metrics.read_materialized_bytes += materialized;
            metrics.source_raster_io_calls += read.raster_io_calls;
            metrics.source_read_decode_ms += read.read_decode_ms;
            metrics.read_bound_peak_bytes = metrics.read_bound_peak_bytes.max(bound);
            ensure!(
                raw.width == cw && raw.height == ch && raw.bands.len() == end - first,
                "ordered source returned wrong raw window"
            );
            for (i, band) in raw.bands.iter().enumerate() {
                let scalar = raw_meta.bands[bands[first + i]].scalar_type.byte_width();
                ensure!(
                    band.samples_le.len() == cw * ch * scalar && band.mask.len() == cw * ch,
                    "ordered source returned wrong typed bytes"
                );
                metrics.raw_sample_bytes += band.samples_le.len() as u64;
                metrics.raw_mask_bytes += band.mask.len() as u64;
            }
            let reduction = Instant::now();
            for (reference, cursor) in cursors.iter_mut().enumerate() {
                let mut visited = 0;
                while let Some((gy, gx)) = cursor.point() {
                    if gy >= row + ch || gx >= cx + cw {
                        break;
                    }
                    ensure!(
                        gy >= row && gx >= cx,
                        "ordered selection cursor moved backwards"
                    );
                    if visited % 1024 == 0 {
                        check_cancel(cancel)?;
                    }
                    visited += 1;
                    let local = (gy - row) * cw + gx - cx;
                    for (i, band) in raw.bands.iter().enumerate() {
                        let scalar = raw_meta.bands[bands[first + i]].scalar_type.byte_width();
                        let at = local * scalar;
                        let value = if scalar == 4 {
                            f32::from_le_bytes(band.samples_le[at..at + 4].try_into()?) as f64
                        } else {
                            f64::from_le_bytes(band.samples_le[at..at + 8].try_into()?)
                        };
                        partials[reference * bands.len() + first + i].add(value, nodata[first + i]);
                    }
                    cursor.advance();
                }
            }
            metrics.reduction_ms += reduction.elapsed().as_secs_f64() * 1000.;
        }
        first = end;
    }
    let mut rows = Vec::with_capacity(request.polygons.len());
    for (p, polygon) in request.polygons.iter().enumerate() {
        check_cancel(cancel)?;
        let mut results = Vec::with_capacity(bands.len());
        for (b, &id) in bands.iter().enumerate() {
            let mut state = State::default();
            for window in polygon_offsets[p]..polygon_offsets[p + 1] {
                state.merge_window(partials[window * bands.len() + b])?;
            }
            results.push(json!({"id":id,"sum":state.sum,"has_values":state.valid_count>0,"valid_count":state.valid_count,
                "excluded_mask":0,"excluded_nodata":state.excluded_nodata,"excluded_nonfinite":state.excluded_nonfinite,"excluded_negative":state.excluded_negative}));
        }
        rows.push(json!({"id":polygon.id,"bands":results}));
    }
    source.verify_immutable()?;
    check_cancel(cancel)?;
    metrics.elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
    Ok(
        json!({"rows":rows,"complete":true,"numerical_policy":POLICY,"metrics":metrics,
        "provenance":{"selection":"explicit_strictly_ascending_local_indexes_or_runs","geometry_generated":false,
            "values":"original_unscaled_float32_float64","source_masks":"ignored_by_explicit_ordered_policy",
            "filter_order":["nonfinite","exact_nodata","negative"],"window_order":"request_order_left_fold",
            "value_scale_applied":false,"summaries_used":false,"shared_reads":"source_row_stripes_across_overlapping_logical_windows",
            "source_id":source.metadata().source_id,"source_identity":source.identity_descriptor(),"source_layout":source.access_layout()},
        "source_diagnostics_included":false}),
    )
}
