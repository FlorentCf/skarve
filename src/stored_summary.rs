//! Optional sufficient states owned by a serving source. Storage identity and
//! reads remain the reader's responsibility; native coverage/reducers are shared.
use crate::{
    aggregate::Options,
    model::{Grid, Sum},
    persistent::TileSummary,
};
use anyhow::{Context, Result, ensure};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug)]
pub struct SummaryLayout {
    pub tile_edge: usize,
    /// Leaf-first (columns, rows, first record); parent dimensions use ceil/2.
    pub levels: Vec<(usize, usize, usize)>,
}
impl SummaryLayout {
    pub fn new(grid: &Grid, tile_edge: usize, hierarchical: bool) -> Result<Self> {
        grid.validate()?;
        ensure!(
            [16, 32, 64, 128, 256, 512].contains(&tile_edge),
            "unsupported summary tile edge"
        );
        let (mut nx, mut ny, mut offset) = (
            grid.width.div_ceil(tile_edge),
            grid.height.div_ceil(tile_edge),
            0usize,
        );
        let mut levels = Vec::new();
        loop {
            ensure!(levels.len() < 32, "summary level budget exceeded");
            levels.push((nx, ny, offset));
            offset = nx
                .checked_mul(ny)
                .and_then(|n| offset.checked_add(n))
                .context("summary record count overflow")?;
            if !hierarchical || (nx == 1 && ny == 1) {
                break;
            }
            nx = nx.div_ceil(2);
            ny = ny.div_ceil(2);
        }
        Ok(Self { tile_edge, levels })
    }
    pub fn validate(&self, grid: &Grid) -> Result<()> {
        ensure!(
            !self.levels.is_empty() && self.levels.len() <= 32,
            "invalid summary levels"
        );
        let expected = Self::new(grid, self.tile_edge, self.levels.len() > 1)?;
        ensure!(
            self.levels == expected.levels,
            "summary hierarchy does not match native grid"
        );
        Ok(())
    }
    pub fn records(&self) -> Result<usize> {
        let &(nx, ny, first) = self.levels.last().context("missing summary levels")?;
        nx.checked_mul(ny)
            .and_then(|n| first.checked_add(n))
            .context("summary record count overflow")
    }
    pub fn bounds(&self, grid: &Grid, record: usize) -> Result<[usize; 4]> {
        self.validate(grid)?;
        ensure!(record < self.records()?, "summary record out of bounds");
        let (level, &(nx, _, offset)) = self
            .levels
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, _, first))| record >= *first)
            .context("invalid summary record")?;
        let edge = self
            .tile_edge
            .checked_shl(level as u32)
            .context("summary edge overflow")?;
        let local = record - offset;
        let x = (local % nx)
            .checked_mul(edge)
            .context("summary x overflow")?;
        let y = (local / nx)
            .checked_mul(edge)
            .context("summary y overflow")?;
        Ok([
            x,
            y,
            x.saturating_add(edge).min(grid.width),
            y.saturating_add(edge).min(grid.height),
        ])
    }
}

/// Implementations return precisely the requested band order and must validate
/// checksums, immutable generation, state/count bounds and cancellation. A query
/// never follows the original-source provenance to find boundary values.
pub trait StoredSummarySource {
    fn summary_layout(&self) -> &SummaryLayout;
    fn read_summary(
        &self,
        record: usize,
        bands: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Vec<TileSummary>>;
    /// Includes encoded input, decoding scratch and returned states. At most20
    /// bands are requested at once; all allocations are charged before reading.
    fn summary_read_buffer_bound(&self, bands: &[usize]) -> Result<usize>;
}

/// Shared persistent-state validation. On-disk empty extrema are zero; Acc's
/// internal empty extrema are infinities. Compensated components are retained.
pub fn validated_state(
    parts: [f64; 2],
    valid_count: u64,
    min: f64,
    max: f64,
    cells: usize,
) -> Result<TileSummary> {
    let sum = Sum::from_parts(parts)?;
    let valid_count = usize::try_from(valid_count)?;
    ensure!(
        valid_count <= cells
            && min.is_finite()
            && max.is_finite()
            && (valid_count == 0 || min <= max),
        "invalid persistent summary state"
    );
    ensure!(
        valid_count > 0 || (sum.parts() == [0., 0.] && min == 0. && max == 0.),
        "empty persistent summary must have zero state"
    );
    if valid_count > 0 {
        let mean = sum.value() / valid_count as f64;
        let tolerance = 1e-8 + 1e-10 * mean.abs().max(min.abs()).max(max.abs());
        ensure!(
            mean >= min - tolerance && mean <= max + tolerance,
            "persistent summary sum inconsistent with extrema/count"
        );
    }
    Ok(TileSummary {
        sum,
        valid_count,
        min: if valid_count > 0 { min } else { f64::INFINITY },
        max: if valid_count > 0 {
            max
        } else {
            f64::NEG_INFINITY
        },
    })
}

/// Broader source orchestration, with existing20-band reducer/read validation
/// retained for every group. This does not expand resident Raster limits.
pub(crate) fn selected_bands(options: &Options, band_count: usize) -> Result<Vec<usize>> {
    let bands = options.selected_bands(band_count);
    ensure!(
        !bands.is_empty() && bands.len() <= 64,
        "source output band count must be1..64"
    );
    ensure!(
        bands
            .iter()
            .enumerate()
            .all(|(i, b)| *b < band_count && !bands[..i].contains(b)),
        "source band index out of range or duplicate"
    );
    for group in bands.chunks(20) {
        let mut part = options.clone();
        part.bands = group.to_vec();
        part.validate(band_count)?;
    }
    Ok(bands)
}

const BOUNDARY_READ_BYTES: usize = 64 << 20;
// Fixed two-leaf descriptors and temporary window/cache keys. No output raster
// is retained by the queue. Coverage-vector capacities are charged separately.
const BOUNDARY_QUEUE_METADATA_BYTES: usize = 4096;
struct BoundaryLeaf {
    bounds: [usize; 4],
    cells: Vec<crate::coverage::Cell>,
    full_read_bound: usize,
}
impl BoundaryLeaf {
    fn cell_bytes(&self) -> usize {
        self.cells
            .capacity()
            .saturating_mul(std::mem::size_of::<crate::coverage::Cell>())
    }
    fn window(&self) -> [usize; 4] {
        [
            self.bounds[0],
            self.bounds[1],
            self.bounds[2] - self.bounds[0],
            self.bounds[3] - self.bounds[1],
        ]
    }
}
#[derive(Default)]
struct BoundaryReadState {
    raw_tiles: usize,
    raw_cells: usize,
    raw_reads: usize,
    decoded: u64,
    read_decode_ms: f64,
    normalization_ms: f64,
}
fn full_boundary_read_bound(
    source: &dyn crate::source::WindowSource,
    bounds: [usize; 4],
    bands: &[usize],
) -> Option<usize> {
    let limit = source.max_read_bands();
    if !(20..=64).contains(&limit) || bands.len() > limit {
        return None;
    }
    // Eligibility is optional. The unchanged eager group path reports errors
    // and shrinks groups exactly as before if the full selection cannot fit.
    source
        .read_buffer_bound(bounds[2] - bounds[0], bounds[3] - bounds[1], bands)
        .ok()
        .filter(|&bound| bound <= BOUNDARY_READ_BYTES)
}
fn boundary_queue_bytes(first: usize, second: usize) -> usize {
    first
        .saturating_add(second)
        .saturating_add(BOUNDARY_QUEUE_METADATA_BYTES)
}
fn boundary_pair_quota_fits(
    state: &BoundaryReadState,
    windows: &[[usize; 4]],
    band_count: usize,
) -> bool {
    let projected = windows.iter().try_fold(state.decoded, |sum, window| {
        (window[2] as u64)
            .checked_mul(window[3] as u64)
            .and_then(|n| n.checked_mul(band_count as u64))
            .and_then(|n| n.checked_mul(8))
            .and_then(|n| sum.checked_add(n))
    });
    state
        .raw_reads
        .checked_add(windows.len())
        .is_some_and(|n| n <= 16384)
        && projected.is_some_and(|n| n <= 2 << 30)
}
#[allow(clippy::too_many_arguments)]
fn consume_boundary(
    source: &dyn crate::source::WindowSource,
    bands: &[usize],
    leaf: BoundaryLeaf,
    cache_identity: &str,
    cache: &mut crate::tile_cache::TileCache,
    cancel: &AtomicBool,
    accumulators: &mut [crate::aggregate::Acc],
    selected: &mut Sum,
    intersecting: &mut usize,
    metrics: &mut BoundaryReadState,
) -> Result<()> {
    use crate::model::check_cancel;
    use std::sync::Arc;
    const READ_BYTES: usize = BOUNDARY_READ_BYTES;
    check_cancel(cancel)?;
    let meta = source.metadata();
    let BoundaryLeaf { bounds, cells, .. } = leaf;
    let (width, height) = (bounds[2] - bounds[0], bounds[3] - bounds[1]);
    for cell in &cells {
        selected.add(cell.fraction);
        *intersecting += 1;
    }
    metrics.raw_cells += cells.len();
    metrics.raw_tiles += 1;
    let read_band_limit = source.max_read_bands();
    ensure!(
        (20..=64).contains(&read_band_limit),
        "invalid source read band capability"
    );
    let mut group_start = 0;
    while group_start < bands.len() {
        let mut group_end = (group_start + read_band_limit).min(bands.len());
        while source.read_buffer_bound(width, height, &bands[group_start..group_end])? > READ_BYTES
        {
            ensure!(
                bands.len() > 20 && group_end > group_start + 1,
                "source summary boundary read exceeds query budget"
            );
            group_end -= 1;
        }
        let group = &bands[group_start..group_end];
        let key = crate::tile_cache::window_key(
            cache_identity,
            [bounds[0], bounds[1], width, height],
            group,
        );
        let raster = if let Some(raster) = cache.get(&key) {
            raster
        } else {
            metrics.decoded = metrics
                .decoded
                .checked_add((width * height * group.len() * 8) as u64)
                .context("source summary decoded byte overflow")?;
            ensure!(
                metrics.decoded <= 2 << 30 && metrics.raw_reads < 16384,
                "source summary read budget exceeded"
            );
            let (raster, read_metrics) = source.read_selected_window_cancellable(
                bounds[0], bounds[1], width, height, group, READ_BYTES, cancel,
            )?;
            crate::source::validate_window(
                meta,
                &raster,
                [bounds[0], bounds[1], width, height],
                group,
            )?;
            metrics.raw_reads += 1;
            metrics.read_decode_ms += read_metrics.read_decode_ms;
            metrics.normalization_ms += read_metrics.normalization_ms;
            let raster = Arc::new(raster);
            cache.insert(key, Arc::clone(&raster));
            raster
        };
        for (cell_number, cell) in cells.iter().enumerate() {
            if cell_number % 4096 == 0 {
                check_cancel(cancel)?;
            }
            let pos = (cell.row - bounds[1]) * width + cell.col - bounds[0];
            for (offset, band) in raster.bands.iter().enumerate() {
                accumulators[group_start + offset].cell(band, pos, cell.fraction, None, None);
            }
        }
        group_start = group_end;
    }
    Ok(())
}

/// Source-owned hierarchical single query. The rectangle certificate, exact
/// boundary coverage and accumulator are the existing persistent algorithms.
pub(crate) fn measure(
    source: &dyn crate::source::WindowSource,
    geometry: &serde_json::Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
    cache: &mut crate::tile_cache::TileCache,
    open_ms: f64,
) -> Result<serde_json::Value> {
    use crate::{
        aggregate::Acc,
        hierarchy::{GeometryPredicate, Relation},
        model::check_cancel,
    };
    use serde_json::json;
    use std::time::Instant;
    const READ_BYTES: usize = BOUNDARY_READ_BYTES;
    let started = Instant::now();
    check_cancel(cancel)?;
    source.verify_immutable()?;
    ensure!(
        options.summaries_eligible(),
        "source summaries cannot answer requested statistics"
    );
    let provider = source
        .stored_summaries()
        .context("source has no stored summaries")?;
    let meta = source.metadata();
    let bands = selected_bands(options, meta.bands.len())?;
    let layout = provider.summary_layout();
    layout.validate(&meta.grid)?;
    let predicate = GeometryPredicate::new(&meta.grid, geometry, crs, cancel)?;
    let accumulator_bound = Acc::retained_bytes(options)?
        .checked_mul(bands.len())
        .context("summary accumulator bound overflow")?;
    let memory_bound = accumulator_bound
        .saturating_add(READ_BYTES)
        .saturating_add(
            layout
                .tile_edge
                .saturating_mul(layout.tile_edge)
                .saturating_mul(std::mem::size_of::<crate::coverage::Cell>() * 2),
        )
        .saturating_add(2 << 20)
        .saturating_add(predicate.vertices().saturating_mul(8192))
        .saturating_add(source.retained_memory_bound())
        .saturating_add(cache.limit());
    ensure!(
        memory_bound <= crate::model::MAX_BYTES,
        "source summary query memory budget exceeded"
    );
    ensure!(
        layout.records()?.saturating_mul(predicate.vertices()) <= 100_000_000,
        "source summary predicate work budget exceeded"
    );
    let mut accumulators: Vec<_> = bands.iter().map(|_| Acc::new(0, options)).collect();
    let mut pending = if layout.levels.len() == 1 {
        let [x0, y0, x1, y1] = predicate.bounds();
        let count = (x1.div_ceil(layout.tile_edge) - x0 / layout.tile_edge)
            .saturating_mul(y1.div_ceil(layout.tile_edge) - y0 / layout.tile_edge);
        ensure!(count <= 16384, "source flat summary task budget exceeded");
        (y0 / layout.tile_edge..y1.div_ceil(layout.tile_edge))
            .rev()
            .flat_map(|y| {
                (x0 / layout.tile_edge..x1.div_ceil(layout.tile_edge))
                    .rev()
                    .map(move |x| (0, x, y))
            })
            .collect::<Vec<_>>()
    } else {
        vec![(layout.levels.len() - 1, 0, 0)]
    };
    let mut selected = Sum::default();
    let (
        mut intersecting,
        mut candidates,
        mut nodes,
        mut summary_reads,
        mut full,
        mut boundary_work,
    ) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut boundary = BoundaryReadState::default();
    let cache_identity = crate::tile_cache::source_key(source)?;
    let [px0, py0, px1, py1] = predicate.bounds();
    let bounding_leaves = (px1
        .div_ceil(layout.tile_edge)
        .saturating_sub(px0 / layout.tile_edge))
    .saturating_mul(
        py1.div_ceil(layout.tile_edge)
            .saturating_sub(py0 / layout.tile_edge),
    );
    let read_ahead = source.boundary_prefetch_enabled() && bounding_leaves > 1;
    let mut queued: Option<BoundaryLeaf> = None;
    let (mut planning_ms, mut preparation_ms) = (0., 0.);
    let (mut preparation_calls, mut queue_peak_bytes, mut queue_peak_leaves) =
        (0usize, 0usize, 0usize);
    while let Some((level, tx, ty)) = pending.pop() {
        check_cancel(cancel)?;
        let record = layout.levels[level].2 + ty * layout.levels[level].0 + tx;
        let bounds = layout.bounds(&meta.grid, record)?;
        candidates += 1;
        let relation = predicate.classify(bounds);
        if relation == Relation::Outside {
            continue;
        }
        if level > 0 && relation == Relation::Boundary {
            let (nx, ny, _) = layout.levels[level - 1];
            for y in (ty * 2..(ty * 2 + 2).min(ny)).rev() {
                for x in (tx * 2..(tx * 2 + 2).min(nx)).rev() {
                    pending.push((level - 1, x, y));
                }
            }
            continue;
        }
        let (width, height) = (bounds[2] - bounds[0], bounds[3] - bounds[1]);
        if relation == Relation::Inside {
            if let Some(first) = queued.take() {
                consume_boundary(
                    source,
                    &bands,
                    first,
                    &cache_identity,
                    cache,
                    cancel,
                    &mut accumulators,
                    &mut selected,
                    &mut intersecting,
                    &mut boundary,
                )?;
            }
            for (gi, group) in bands.chunks(20).enumerate() {
                ensure!(
                    provider.summary_read_buffer_bound(group)? <= READ_BYTES,
                    "source summary read exceeds query budget"
                );
                let states = provider.read_summary(record, group, cancel)?;
                ensure!(
                    states.len() == group.len(),
                    "source returned incompatible summary band count"
                );
                summary_reads += 1;
                for (offset, state) in states.into_iter().enumerate() {
                    accumulators[gi * 20 + offset].merge_summary(
                        state.sum,
                        state.valid_count,
                        state.min,
                        state.max,
                    );
                }
            }
            selected.add((width * height) as f64);
            intersecting += width * height;
            full += (bounds[2].div_ceil(layout.tile_edge) - bounds[0] / layout.tile_edge)
                * (bounds[3].div_ceil(layout.tile_edge) - bounds[1] / layout.tile_edge);
            nodes += 1;
            continue;
        }
        boundary_work = boundary_work.saturating_add(
            width
                .saturating_mul(height)
                .saturating_mul(predicate.vertices()),
        );
        ensure!(
            boundary_work <= 100_000_000
                && boundary.raw_tiles + usize::from(queued.is_some()) < 16384,
            "source summary boundary work budget exceeded"
        );
        let full_read_bound = if read_ahead {
            let plan_started = Instant::now();
            let bound = full_boundary_read_bound(source, bounds, &bands);
            // Before constructing the next coverage vector, reserve its worst
            // Vec growth plus the retained first leaf and unchanged read bound.
            // The original one-leaf geometry scratch remains separately charged.
            let next_cell_bound = width
                .saturating_mul(height)
                .max(4)
                .saturating_mul(2 * std::mem::size_of::<crate::coverage::Cell>());
            let can_retain = queued.as_ref().is_none_or(|first| {
                bound.is_some_and(|n| {
                    boundary_queue_bytes(first.cell_bytes(), next_cell_bound)
                        .saturating_add(first.full_read_bound.max(n))
                        <= READ_BYTES
                })
            });
            planning_ms += plan_started.elapsed().as_secs_f64() * 1000.;
            if !can_retain {
                if let Some(first) = queued.take() {
                    consume_boundary(
                        source,
                        &bands,
                        first,
                        &cache_identity,
                        cache,
                        cancel,
                        &mut accumulators,
                        &mut selected,
                        &mut intersecting,
                        &mut boundary,
                    )?;
                }
            }
            bound
        } else {
            None
        };
        let cells = predicate.boundary_cells(bounds, cancel)?;
        if cells.is_empty() {
            continue;
        }
        let leaf = BoundaryLeaf {
            bounds,
            cells,
            full_read_bound: full_read_bound.unwrap_or(0),
        };
        if !read_ahead {
            consume_boundary(
                source,
                &bands,
                leaf,
                &cache_identity,
                cache,
                cancel,
                &mut accumulators,
                &mut selected,
                &mut intersecting,
                &mut boundary,
            )?;
            continue;
        }
        let plan_started = Instant::now();
        if let Some(first) = queued.as_ref() {
            // Confirm actual Vec capacities as well as the pre-construction
            // reservation before any read while both leaves are retained.
            ensure!(
                boundary_queue_bytes(first.cell_bytes(), leaf.cell_bytes())
                    .saturating_add(first.full_read_bound.max(leaf.full_read_bound))
                    <= READ_BYTES,
                "source summary boundary queue capacity exceeds read reservation"
            );
        }
        let key = crate::tile_cache::window_key(&cache_identity, leaf.window(), &bands);
        let queueable = full_read_bound.is_some()
            && boundary_queue_bytes(leaf.cell_bytes(), 0).saturating_add(leaf.full_read_bound)
                <= READ_BYTES
            && !cache.contains(&key);
        planning_ms += plan_started.elapsed().as_secs_f64() * 1000.;
        if !queueable {
            if let Some(first) = queued.take() {
                consume_boundary(
                    source,
                    &bands,
                    first,
                    &cache_identity,
                    cache,
                    cancel,
                    &mut accumulators,
                    &mut selected,
                    &mut intersecting,
                    &mut boundary,
                )?;
            }
            consume_boundary(
                source,
                &bands,
                leaf,
                &cache_identity,
                cache,
                cancel,
                &mut accumulators,
                &mut selected,
                &mut intersecting,
                &mut boundary,
            )?;
            continue;
        }
        if let Some(first) = queued.take() {
            let plan_started = Instant::now();
            let charge = boundary_queue_bytes(first.cell_bytes(), leaf.cell_bytes());
            let windows = [first.window(), leaf.window()];
            let first_key = crate::tile_cache::window_key(&cache_identity, windows[0], &bands);
            let can_prepare = charge
                .saturating_add(first.full_read_bound.max(leaf.full_read_bound))
                <= READ_BYTES
                && !cache.contains(&first_key)
                && !cache.contains(&key)
                && boundary_pair_quota_fits(&boundary, &windows, bands.len());
            queue_peak_bytes = queue_peak_bytes.max(charge);
            queue_peak_leaves = queue_peak_leaves.max(2);
            planning_ms += plan_started.elapsed().as_secs_f64() * 1000.;
            if can_prepare {
                check_cancel(cancel)?;
                let prepare_started = Instant::now();
                source.prefetch_boundary_windows(&windows, &bands, READ_BYTES - charge, cancel)?;
                preparation_ms += prepare_started.elapsed().as_secs_f64() * 1000.;
                preparation_calls += 1;
                check_cancel(cancel)?;
            }
            // The original compensated selected/summary/raw merge order is
            // preserved regardless of preparation, cache hits or source layout.
            consume_boundary(
                source,
                &bands,
                first,
                &cache_identity,
                cache,
                cancel,
                &mut accumulators,
                &mut selected,
                &mut intersecting,
                &mut boundary,
            )?;
            consume_boundary(
                source,
                &bands,
                leaf,
                &cache_identity,
                cache,
                cancel,
                &mut accumulators,
                &mut selected,
                &mut intersecting,
                &mut boundary,
            )?;
        } else {
            queue_peak_bytes = queue_peak_bytes.max(boundary_queue_bytes(leaf.cell_bytes(), 0));
            queue_peak_leaves = queue_peak_leaves.max(1);
            queued = Some(leaf);
        }
    }
    if let Some(first) = queued.take() {
        consume_boundary(
            source,
            &bands,
            first,
            &cache_identity,
            cache,
            cancel,
            &mut accumulators,
            &mut selected,
            &mut intersecting,
            &mut boundary,
        )?;
    }
    source.verify_immutable()?;
    let results = accumulators
        .into_iter()
        .zip(&bands)
        .map(|(acc, &bi)| {
            acc.finish_cancellable(
                bi,
                selected.value(),
                predicate.polygon_area(),
                intersecting,
                meta.bands[bi].unit.clone(),
                options,
                cancel,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"bands":results,"grid":meta.grid,"source_id":meta.source_id,"mode":"native_grid_planar","precision":"f64_compensated","strategy":"source_stored_hierarchy","plan_reused":false,
        "work":{"candidate_tiles":candidates,"summary_nodes":nodes,"summary_records_read":summary_reads,"summary_tiles":full,"raw_tiles":boundary.raw_tiles,"boundary_tiles":boundary.raw_tiles,"raw_positive_cells":boundary.raw_cells,"boundary_read_ahead":read_ahead,"boundary_preparation_calls":preparation_calls,"boundary_queue_peak_leaves":queue_peak_leaves,"boundary_queue_peak_capacity_bytes":queue_peak_bytes,"eligible_raw_interior_tiles_avoided":full,"buffer_bound_bytes":memory_bound,"full_resolution_plan":false,"band_groups":bands.len().div_ceil(20)},
        "streaming":{"tiles_read":boundary.raw_reads,"decoded_value_bytes":boundary.decoded,"cache_resident_bytes":cache.bytes(),"cache_budget_bytes":cache.limit(),"tile_size":layout.tile_edge},
        "timing_ms":{"source_open":open_ms,"read_decode":boundary.read_decode_ms,"normalization":boundary.normalization_ms,"boundary_planning":planning_ms,"boundary_preparation":preparation_ms,"total":started.elapsed().as_secs_f64()*1000.+open_ms}}),
    )
}

#[cfg(test)]
mod boundary_queue_tests {
    use super::*;
    #[test]
    fn pair_projection_checks_read_and_decoded_limits_before_prefetch() {
        let windows = [[0, 0, 128, 128], [128, 0, 128, 128]];
        let bytes = 2 * 128 * 128 * 40 * 8;
        let at_limit = BoundaryReadState {
            raw_reads: 16382,
            decoded: (2 << 30) - bytes,
            ..Default::default()
        };
        assert!(boundary_pair_quota_fits(&at_limit, &windows, 40));
        let reads = BoundaryReadState {
            raw_reads: 16383,
            ..Default::default()
        };
        assert!(!boundary_pair_quota_fits(&reads, &windows, 40));
        let decoded = BoundaryReadState {
            decoded: at_limit.decoded + 1,
            ..Default::default()
        };
        assert!(!boundary_pair_quota_fits(&decoded, &windows, 40));
        let overflow = [[0, 0, usize::MAX, usize::MAX], [0, 0, 1, 1]];
        assert!(!boundary_pair_quota_fits(
            &BoundaryReadState::default(),
            &overflow,
            64
        ));
    }
}
