//! Bounded local-file execution: compile once, partition coverage, decode one tile at a time.
use crate::{
    aggregate::{self, Acc, Options},
    coverage::{self, Cell, Plan, Span},
    io::LocalSource,
    model::{Sum, check_cancel},
    source::WindowSource,
    tile_cache::TileCache,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

const TILE: usize = 256;
const TILE_BYTES: usize = 64 * 1024 * 1024;
const PLAN_BYTES: usize = 256 * 1024 * 1024;
const MAX_TILES: usize = 16_384;

#[derive(Default)]
struct Selection {
    spans: Vec<Span>,
    cells: Vec<Cell>,
    selected: Sum,
    intersecting: usize,
}
/// Source data stays on disk; one bounded 256/512-edge native window is read at a time.
/// Limits: 64 MiB tile/decode allocation, 256 MiB coverage partition accounting, 16384 tiles.
pub fn measure_local_file(
    path: &str,
    geometry: &Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Value> {
    let start = Instant::now();
    check_cancel(cancel)?;
    let source = LocalSource::open(path)?;
    let open_ms = start.elapsed().as_secs_f64() * 1000.;
    measure_cached_source(
        &source,
        geometry,
        crs,
        options,
        cancel,
        &mut TileCache::default(),
        open_ms,
    )
}

/// Registered source path with bounded decoded data reuse; geometry always compiled anew.
pub fn measure_cached_source(
    source: &dyn WindowSource,
    geometry: &Value,
    crs: &str,
    options: &Options,
    cancel: &AtomicBool,
    cache: &mut TileCache,
    open_ms: f64,
) -> Result<Value> {
    if source.stored_summaries().is_some() && options.summaries_eligible() {
        return crate::stored_summary::measure(
            source, geometry, crs, options, cancel, cache, open_ms,
        );
    }
    let start = Instant::now();
    let source_access_before = source.diagnostics();
    source.verify_immutable()?;
    let identity_ms = start.elapsed().as_secs_f64() * 1000.;
    let band_indices =
        crate::stored_summary::selected_bands(options, source.metadata().bands.len())?;
    ensure!(
        band_indices
            .iter()
            .all(|&b| b < source.metadata().bands.len()),
        "band index out of range"
    );
    ensure!(
        band_indices.len() <= 20 || options.weight_band.is_none(),
        "more than20 output bands currently require unweighted source execution"
    );
    ensure!(
        band_indices
            .iter()
            .enumerate()
            .all(|(i, b)| !band_indices[..i].contains(b)),
        "duplicate bands not supported"
    );
    if let Some(band) = options.weight_band {
        ensure!(
            band < source.metadata().bands.len(),
            "weight band out of range"
        );
    }
    let mut read_bands = band_indices.clone();
    if let Some(band) = options.weight_band {
        if !read_bands.contains(&band) {
            read_bands.push(band);
        }
    }
    // Compact decoded tile bands retain original requested output order.
    let tile_options = Options {
        statistics: options.statistics.clone(),
        category_values: options.category_values.clone(),
        quantiles: options.quantiles.clone(),
        quantile_max_samples: options.quantile_max_samples,
        bands: (0..band_indices.len()).collect(),
        histogram_edges: options.histogram_edges.clone(),
        weight_band: options.weight_band.map(|band| {
            read_bands
                .iter()
                .position(|&b| b == band)
                .expect("weight band included")
        }),
    };
    if let Some(edges) = &options.histogram_edges {
        ensure!(
            (2..=257).contains(&edges.len())
                && edges.iter().all(|v| v.is_finite())
                && edges.windows(2).all(|p| p[0] < p[1]),
            "histogram edges must be finite strictly increasing, 2..257 entries"
        );
    }
    let plan = coverage::compile_with_budget(
        &source.metadata().grid,
        geometry,
        crs,
        "scanline",
        cancel,
        PLAN_BYTES,
    )?;
    let partition_start = Instant::now();
    // Query geometry is already exact and compiled. Base occupied tiles expose
    // real window diversity without estimating coverage from polygon bboxes.
    // Reserve both sets/vector/policy scratch before allocation; if this optional
    // planning state does not fit, preserve the established fixed-edge path.
    let planner_reservation = MAX_TILES * 320 + 4096;
    let layout_candidate = read_bands.iter().any(|&i| {
        let (w, h) = source.metadata().bands[i].block_size;
        w == h && w.is_power_of_two() && w > TILE
    });
    let source_window_policy = if read_bands.len() <= 20
        && layout_candidate
        && plan
            .bytes()
            .saturating_mul(2)
            .saturating_add(planner_reservation)
            <= PLAN_BYTES
    {
        let mut occupied = std::collections::BTreeSet::new();
        for span in &plan.spans {
            check_cancel(cancel)?;
            for tx in span.start / TILE..(span.end - 1) / TILE + 1 {
                ensure!(
                    occupied.len() < MAX_TILES || occupied.contains(&(span.row / TILE, tx)),
                    "streamed tile-count budget exceeded"
                );
                occupied.insert((span.row / TILE, tx));
            }
        }
        for (n, cell) in plan.cells.iter().enumerate() {
            if n % 4096 == 0 {
                check_cancel(cancel)?;
            }
            let key = (cell.row / TILE, cell.col / TILE);
            ensure!(
                occupied.len() < MAX_TILES || occupied.contains(&key),
                "streamed tile-count budget exceeded"
            );
            occupied.insert(key);
        }
        let tiles = occupied.iter().copied().collect::<Vec<_>>();
        Some(crate::source::select_window_edge(
            source,
            TILE,
            &read_bands,
            &tiles,
            TILE_BYTES,
        )?)
    } else {
        None
    };
    let tile_edge = source_window_policy
        .as_ref()
        .map_or(TILE, |p| p.selected_edge);
    let mut selections: BTreeMap<(usize, usize), Selection> = BTreeMap::new();
    let mut partition_bytes = plan
        .bytes()
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(4096 + 256))
        .context("streamed plan size overflow")?;
    ensure!(
        partition_bytes <= PLAN_BYTES,
        "streamed plan memory budget exceeded"
    );
    for span in &plan.spans {
        check_cancel(cancel)?;
        let mut col = span.start;
        while col < span.end {
            let key = (span.row / tile_edge, col / tile_edge);
            if !selections.contains_key(&key) {
                ensure!(
                    selections.len() < MAX_TILES,
                    "streamed tile-count budget exceeded"
                );
                partition_bytes += 256;
            }
            partition_bytes = partition_bytes
                .checked_add(std::mem::size_of::<Span>() * 2)
                .context("streamed plan size overflow")?;
            ensure!(
                partition_bytes <= PLAN_BYTES,
                "streamed plan memory budget exceeded"
            );
            let selected = selections.entry(key).or_default();
            let end = span.end.min((key.1 + 1) * tile_edge);
            selected.spans.push(Span {
                row: span.row % tile_edge,
                start: col % tile_edge,
                end: end - key.1 * tile_edge,
            });
            selected.selected.add((end - col) as f64);
            selected.intersecting += end - col;
            col = end;
        }
    }
    for (n, cell) in plan.cells.iter().enumerate() {
        if n % 4096 == 0 {
            check_cancel(cancel)?;
        }
        let key = (cell.row / tile_edge, cell.col / tile_edge);
        if !selections.contains_key(&key) {
            ensure!(
                selections.len() < MAX_TILES,
                "streamed tile-count budget exceeded"
            );
            partition_bytes += 256;
        }
        partition_bytes = partition_bytes
            .checked_add(std::mem::size_of::<Cell>() * 2)
            .context("streamed plan size overflow")?;
        ensure!(
            partition_bytes <= PLAN_BYTES,
            "streamed plan memory budget exceeded"
        );
        let selected = selections.entry(key).or_default();
        selected.cells.push(Cell {
            row: cell.row % tile_edge,
            col: cell.col % tile_edge,
            fraction: cell.fraction,
        });
        selected.selected.add(cell.fraction);
        selected.intersecting += 1;
    }
    let partition_ms = partition_start.elapsed().as_secs_f64() * 1000.;
    let tile_count = selections.len();
    let reducer_bytes = Acc::retained_bytes(&tile_options)?
        .checked_mul(band_indices.len() * 2)
        .and_then(|n| n.checked_add(Acc::transient_bytes(&tile_options)))
        .context("streamed reducer memory overflow")?;
    ensure!(
        partition_bytes
            .saturating_add(reducer_bytes)
            .saturating_add(TILE_BYTES)
            .saturating_add(cache.limit())
            <= crate::model::MAX_BYTES,
        "streamed reducer memory budget exceeded"
    );
    let mut accumulators: Vec<_> = band_indices
        .iter()
        .map(|_| {
            Acc::new(
                options
                    .histogram_edges
                    .as_ref()
                    .map_or(0, |edges| edges.len() - 1),
                &tile_options,
            )
        })
        .collect();
    let mut io_ms = open_ms;
    let mut aggregation_ms = 0.;
    let mut decoded_bytes: u64 = 0;
    let mut max_tile_resident_bytes = 0;
    let mut cache_hits = 0;
    let mut cache_misses = 0;
    let mut cache_ms = 0.;
    let mut read_decode_ms = 0.;
    let mut normalization_ms = 0.;
    let mut raster_io_calls = 0;
    let cache_identity = crate::tile_cache::source_key(source)?;
    let cache_before = cache.stats();
    for ((ty, tx), mut selection) in selections {
        check_cancel(cancel)?;
        let (x, y) = (tx * tile_edge, ty * tile_edge);
        let width = tile_edge.min(source.metadata().grid.width - x);
        let height = tile_edge.min(source.metadata().grid.height - y);
        let mut tile_plan = None;
        let mut group_start = 0;
        let read_band_limit = source.max_read_bands();
        ensure!(
            (20..=64).contains(&read_band_limit),
            "invalid source read band capability"
        );
        while group_start < band_indices.len() {
            let mut group_end = (group_start + read_band_limit).min(band_indices.len());
            if band_indices.len() > 20 {
                // The source capability bounds physical read width, while actual
                // decoder buffers can require a smaller group. Keep geometry and each
                // band's numerical order; change only the admitted read width.
                // A one-band physical floor above the budget still rejects.
                while source.read_buffer_bound(
                    width,
                    height,
                    &band_indices[group_start..group_end],
                )? > TILE_BYTES
                {
                    ensure!(
                        group_end > group_start + 1,
                        "source read exceeds streaming memory budget even for one band"
                    );
                    group_end -= 1;
                }
            }
            let output_group = &band_indices[group_start..group_end];
            let grouped_read;
            let read_bands = if band_indices.len() <= 20 {
                &read_bands
            } else {
                grouped_read = output_group.to_vec();
                &grouped_read
            };
            let mut group_options = tile_options.clone();
            group_options.bands = (0..output_group.len()).collect();
            let io_start = Instant::now();
            let key =
                crate::tile_cache::window_key(&cache_identity, [x, y, width, height], &read_bands);
            let cached = cache.get(&key);
            cache_ms += io_start.elapsed().as_secs_f64() * 1000.;
            let raster = if let Some(raster) = cached {
                cache_hits += 1;
                raster
            } else {
                cache_misses += 1;
                if band_indices.len() <= 20 {
                    ensure!(
                        source.read_buffer_bound(width, height, &read_bands)? <= TILE_BYTES,
                        "source read exceeds streaming memory budget"
                    );
                }
                let (raster, metrics) = source.read_selected_window_cancellable(
                    x,
                    y,
                    width,
                    height,
                    &read_bands,
                    TILE_BYTES,
                    cancel,
                )?;
                crate::source::validate_window(
                    source.metadata(),
                    &raster,
                    [x, y, width, height],
                    &read_bands,
                )?;
                read_decode_ms += metrics.read_decode_ms;
                normalization_ms += metrics.normalization_ms;
                raster_io_calls += metrics.raster_io_calls;
                decoded_bytes +=
                    (raster.grid.width * raster.grid.height * raster.bands.len() * 8) as u64;
                let raster = Arc::new(raster);
                let cache_started = Instant::now();
                cache.insert(key, Arc::clone(&raster));
                cache_ms += cache_started.elapsed().as_secs_f64() * 1000.;
                raster
            };
            io_ms += io_start.elapsed().as_secs_f64() * 1000.;
            check_cancel(cancel)?;
            max_tile_resident_bytes = max_tile_resident_bytes.max(raster.bytes());
            let tile_plan = tile_plan.get_or_insert_with(|| Plan {
                grid_id: raster.grid.identity(),
                grid: raster.grid.clone(),
                identity: format!("{}:tile:{ty}:{tx}", plan.identity),
                spans: std::mem::take(&mut selection.spans),
                cells: std::mem::take(&mut selection.cells),
                selected: selection.selected.value(),
                polygon_area: selection.selected.value(),
                intersecting: selection.intersecting,
                strategy: "scanline_streamed".to_string(),
                validation_ms: 0.,
                compilation_ms: 0.,
            });
            let aggregate_start = Instant::now();
            // A wider admitted source read never widens the resident reducer
            // contract. Each reducer sees at most20 references in the same
            // raster; no values or masks are cloned between these groups.
            for first in (0..output_group.len()).step_by(20) {
                let end = (first + 20).min(output_group.len());
                group_options.bands = (first..end).collect();
                let results =
                    aggregate::measure_states(&raster, &tile_plan, &group_options, None, cancel)?;
                for (accumulator, result) in accumulators[group_start + first..group_start + end]
                    .iter_mut()
                    .zip(results)
                {
                    accumulator.merge_acc(result)?;
                }
            }
            aggregation_ms += aggregate_start.elapsed().as_secs_f64() * 1000.;
            group_start = group_end;
        }
    }
    source.verify_immutable()?;
    let cache_after = cache.stats();
    let source_access_after = source.diagnostics();
    let network_delta = |key: &str| {
        source_access_after["remote"][key]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(source_access_before["remote"][key].as_u64().unwrap_or(0))
    };
    let bands = accumulators
        .into_iter()
        .zip(&band_indices)
        .map(|(accumulator, &bi)| {
            let unit = source.metadata().bands[bi].unit.clone();
            accumulator.finish_cancellable(
                bi,
                plan.selected,
                plan.polygon_area,
                plan.intersecting,
                unit,
                &tile_options,
                cancel,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"bands":bands,"grid":source.metadata().grid,"source_id":source.metadata().source_id,
        "mode":"native_grid_planar","precision":"f64_compensated","strategy":"scanline_streamed",
        "plan":plan.diagnostics(),"plan_reused":false,
        "streaming":{"tile_size":tile_edge,"source_window_policy":source_window_policy,"source_window_planner_reservation_bytes":if source_window_policy.is_some() {planner_reservation} else {0},"tiles_selected":tile_count,"tiles_read":cache_misses,"decoded_value_bytes":decoded_bytes,
            "read_bands":read_bands,"cache_hits":cache_hits,"cache_misses":cache_misses,
            "cache_resident_bytes":cache.bytes(),"cache_budget_bytes":cache.limit(),"cache_entries":cache.len(),
            "cache_hit_payload_bytes":cache_after.hit_payload_bytes-cache_before.hit_payload_bytes,
            "cache_admitted_payload_bytes":cache_after.admitted_payload_bytes-cache_before.admitted_payload_bytes,
            "cache_evictions":cache_after.evictions-cache_before.evictions,
            "cache_evicted_accounted_bytes":cache_after.evicted_accounted_bytes-cache_before.evicted_accounted_bytes,
            "cache_admission_rejections":cache_after.admission_rejections-cache_before.admission_rejections,
            "raster_io_calls":raster_io_calls,"network_requests":network_delta("requests"),
            "network_bytes":network_delta("received_bytes"),"network_get_requests":network_delta("get_requests"),
            "network_head_requests":network_delta("head_requests"),
            "physical_read_bytes":Value::Null,"decompression_ms":Value::Null,
            "max_tile_resident_bytes":max_tile_resident_bytes,"tile_allocation_budget":TILE_BYTES,
            "reducer_bound_bytes":reducer_bytes,"partition_accounted_bytes":partition_bytes,"partition_budget":PLAN_BYTES},
        "timing_ms":{"validation":plan.validation_ms,"compilation":plan.compilation_ms,"partition":partition_ms,
            "source_open":open_ms,"source_identity":identity_ms,"cache":cache_ms,"read_decode":read_decode_ms,
            "normalization":normalization_ms,"io":io_ms,"aggregation":aggregation_ms,"total":start.elapsed().as_secs_f64()*1000.+open_ms}}),
    )
}
