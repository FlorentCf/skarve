//! Explicit HM population compatibility: spherical surface area along straight
//! lon/lat edges, independent of the engine's native planar statistics.
use crate::{
    coverage::{self, Cell, Span},
    model::{Grid, Sum, check_cancel},
    persistent::{BoundarySource, PersistedIndex},
    source::{WindowSource, validate_window},
};
use anyhow::{Context, Result, ensure};
use num_rational::BigRational;
use num_traits::ToPrimitive;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::atomic::AtomicBool, time::Instant};

pub const POLICY: &str = "hm_straight_lonlat_spherical_v1";
pub const RESERVATION_BYTES: usize = 256 * 1024 * 1024;
const GEOMETRY_BYTES: usize = 128 * 1024 * 1024;
const MAX_WORK: usize = 50_000_000;
const MAX_TILES: usize = 4096;
type Point = [f64; 2];
type Rings = Vec<(Vec<Point>, f64)>;

#[derive(Default)]
struct Selection {
    spans: Vec<Span>,
    cells: Vec<Cell>,
}
#[derive(Default)]
struct Work {
    vertices: usize,
    weights: usize,
}
impl Work {
    fn charge(&mut self, n: usize, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        self.vertices = self
            .vertices
            .checked_add(n)
            .context("HM clipping work overflow")?;
        ensure!(
            self.vertices <= MAX_WORK,
            "HM clipping vertex budget exceeded"
        );
        Ok(())
    }
}
fn clip(
    ring: &[Point],
    axis: usize,
    bound: f64,
    lower: bool,
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Vec<Point>> {
    work.charge(ring.len(), cancel)?;
    let mut out = Vec::with_capacity(ring.len().saturating_mul(2));
    let Some(mut prev) = ring.last().copied() else {
        return Ok(out);
    };
    let inside = |p: Point| {
        if lower {
            p[axis] >= bound
        } else {
            p[axis] <= bound
        }
    };
    for &next in ring {
        if inside(prev) != inside(next) {
            let t = (bound - prev[axis]) / (next[axis] - prev[axis]);
            let mut point = [
                prev[0] + t * (next[0] - prev[0]),
                prev[1] + t * (next[1] - prev[1]),
            ];
            point[axis] = bound;
            out.push(point);
        }
        if inside(next) {
            out.push(next);
        }
        prev = next;
    }
    Ok(out)
}
fn clip_rect(
    ring: &[Point],
    rect: [f64; 4],
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Vec<Point>> {
    let a = clip(ring, 0, rect[0], true, work, cancel)?;
    let b = clip(&a, 0, rect[2], false, work, cancel)?;
    let c = clip(&b, 1, rect[1], true, work, cancel)?;
    clip(&c, 1, rect[3], false, work, cancel)
}
/// Twice the unit-sphere area. Radius and common factor cancel in fractions.
/// Green's theorem integrates sin(latitude) along straight lon/lat edges.
/// The reference sine is subtracted with a stable trig identity, not subtraction
/// of two nearly equal sine values; sinc-1 uses its local series.
fn ring_area(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.;
    }
    let rad = std::f64::consts::PI / 180.;
    let reference = ring[0][1];
    let mut previous = *ring.last().unwrap();
    let mut sum = Sum::default();
    for current in ring {
        let delta = current[1] - previous[1];
        let half = delta * rad * 0.5;
        let sm1 = if half.abs() < 1e-3 {
            let h2 = half * half;
            h2 * (-1. / 6. + h2 * (1. / 120. + h2 * (-1. / 5040. + h2 / 362880.)))
        } else {
            half.sin() / half - 1.
        };
        let midpoint_offset = (previous[1] - reference) + delta * 0.5;
        let sine_difference = 2.
            * ((reference + midpoint_offset * 0.5) * rad).cos()
            * (midpoint_offset * rad * 0.5).sin();
        let midpoint_sine = ((previous[1] + delta * 0.5) * rad).sin();
        sum.add(2. * (current[0] - previous[0]) * rad * (sine_difference + midpoint_sine * sm1));
        previous = *current;
    }
    sum.value().abs()
}

type QPoint = [BigRational; 2];
type QRings = Vec<(Vec<QPoint>, f64)>;
fn qcheck(q: &BigRational) -> Result<()> {
    ensure!(
        q.numer().bits() <= 8192 && q.denom().bits() <= 8192,
        "HM exact clipping integer budget exceeded"
    );
    Ok(())
}
fn qclip(
    ring: &[QPoint],
    axis: usize,
    bound: &BigRational,
    lower: bool,
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Vec<QPoint>> {
    work.charge(ring.len(), cancel)?;
    let mut out = Vec::with_capacity(ring.len().saturating_mul(2));
    let Some(mut previous) = ring.last() else {
        return Ok(out);
    };
    let inside = |p: &QPoint| {
        if lower {
            p[axis] >= *bound
        } else {
            p[axis] <= *bound
        }
    };
    for current in ring {
        if inside(previous) != inside(current) {
            let t = (bound - &previous[axis]) / (&current[axis] - &previous[axis]);
            qcheck(&t)?;
            let mut point = [
                &previous[0] + &t * (&current[0] - &previous[0]),
                &previous[1] + &t * (&current[1] - &previous[1]),
            ];
            point[axis] = bound.clone();
            qcheck(&point[0])?;
            qcheck(&point[1])?;
            out.push(point);
        }
        if inside(current) {
            out.push(current.clone());
        }
        previous = current;
    }
    Ok(out)
}
fn qclip_rect(
    ring: &[QPoint],
    rect: [f64; 4],
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Vec<QPoint>> {
    let q: Vec<_> = rect
        .into_iter()
        .map(|x| BigRational::from_float(x).expect("validated finite world bounds"))
        .collect();
    let a = qclip(ring, 0, &q[0], true, work, cancel)?;
    let b = qclip(&a, 0, &q[2], false, work, cancel)?;
    let c = qclip(&b, 1, &q[1], true, work, cancel)?;
    qclip(&c, 1, &q[3], false, work, cancel)
}
fn qarea(ring: &[QPoint]) -> Result<f64> {
    if ring.len() < 3 {
        return Ok(0.);
    }
    let reference = &ring[0][1];
    let reference_f = reference
        .to_f64()
        .context("HM exact latitude unrepresentable")?;
    let mut previous = ring.last().unwrap();
    let mut sum = Sum::default();
    let rad = std::f64::consts::PI / 180.;
    for current in ring {
        let delta = &current[1] - &previous[1];
        let midpoint_offset =
            &previous[1] - reference + &delta / BigRational::from_integer(2.into());
        let h = delta
            .to_f64()
            .context("HM exact latitude delta unrepresentable")?
            * rad
            * 0.5;
        let offset = midpoint_offset
            .to_f64()
            .context("HM exact midpoint delta unrepresentable")?;
        let dx = (&current[0] - &previous[0])
            .to_f64()
            .context("HM exact longitude delta unrepresentable")?;
        let sm1 = if h.abs() < 1e-3 {
            let h2 = h * h;
            h2 * (-1. / 6. + h2 * (1. / 120. + h2 * (-1. / 5040. + h2 / 362880.)))
        } else {
            h.sin() / h - 1.
        };
        let difference =
            2. * ((reference_f + offset * 0.5) * rad).cos() * (offset * rad * 0.5).sin();
        sum.add(2. * dx * rad * (difference + ((reference_f + offset) * rad).sin() * sm1));
        previous = current;
    }
    Ok(sum.value().abs())
}
fn qoverlap(rings: &QRings, rect: [f64; 4], work: &mut Work, cancel: &AtomicBool) -> Result<f64> {
    let mut sum = Sum::default();
    for (ring, sign) in rings {
        sum.add(sign * qarea(&qclip_rect(ring, rect, work, cancel)?)?);
    }
    Ok(sum.value())
}
fn area(rings: &Rings) -> f64 {
    let mut sum = Sum::default();
    for (ring, sign) in rings {
        sum.add(sign * ring_area(ring));
    }
    sum.value()
}
fn parse_original(input: &Value) -> Result<Rings> {
    let geom = if input["type"] == "Feature" {
        &input["geometry"]
    } else {
        input
    };
    let coordinates = geom["coordinates"]
        .as_array()
        .context("HM polygon coordinates required")?;
    let polygons: Vec<&Value> = if geom["type"] == "Polygon" {
        vec![&geom["coordinates"]]
    } else {
        coordinates.iter().collect()
    };
    let mut result = Vec::new();
    for polygon in polygons {
        for (i, ring) in polygon
            .as_array()
            .context("HM polygon rings required")?
            .iter()
            .enumerate()
        {
            let values = ring.as_array().context("HM ring required")?;
            let mut points = Vec::with_capacity(values.len().saturating_sub(1));
            for p in &values[..values.len().saturating_sub(1)] {
                points.push([
                    p[0].as_f64().context("HM longitude required")?,
                    p[1].as_f64().context("HM latitude required")?,
                ]);
            }
            result.push((points, if i == 0 { 1. } else { -1. }));
        }
    }
    Ok(result)
}
fn bounds(grid: &Grid, x: usize, y: usize, width: usize, height: usize) -> [f64; 4] {
    [
        grid.transform[0] + x as f64 * grid.transform[1],
        grid.transform[3] + (y + height) as f64 * grid.transform[5],
        grid.transform[0] + (x + width) as f64 * grid.transform[1],
        grid.transform[3] + y as f64 * grid.transform[5],
    ]
}
fn fraction(
    rings: &Rings,
    exact: Option<&QRings>,
    rect: [f64; 4],
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<f64> {
    let mut covered = Sum::default();
    if let Some(q) = exact {
        covered.add(qoverlap(q, rect, work, cancel)?);
    } else {
        for (ring, sign) in rings {
            covered.add(sign * ring_area(&clip_rect(ring, rect, work, cancel)?));
        }
    }
    let rad = std::f64::consts::PI / 180.;
    let cell_area = 4.
        * (rect[2] - rect[0])
        * rad
        * (((rect[3] + rect[1]) * rad * 0.5).cos())
        * (((rect[3] - rect[1]) * rad * 0.5).sin());
    ensure!(
        cell_area.is_finite() && cell_area > 0.,
        "HM cell spherical area unrepresentable"
    );
    let ratio = covered.value() / cell_area;
    ensure!(
        ratio.is_finite() && (-1e-8..=1. + 1e-8).contains(&ratio),
        "HM spherical fraction outside arithmetic tolerance"
    );
    let ratio = ratio.clamp(0., 1.);
    work.weights += 1;
    // This is the pinned HM compatibility rule, not the strict planar rule.
    Ok(if ratio >= 1. - 1e-10 { 1. } else { ratio })
}

/// Only source band zero is supported: the registered source must expose exactly
/// one normalized band. Other statistics, weight policies and area models do not
/// enter this interface. Caller reserves RESERVATION_BYTES plus retained source.
pub fn measure(
    source: &dyn WindowSource,
    input: &Value,
    index_path: Option<&str>,
    expected_build_id: Option<&str>,
    read_bytes: usize,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    ensure!(
        (1 << 20..=64 << 20).contains(&read_bytes),
        "HM reader budget must be1..64MiB"
    );
    check_cancel(cancel)?;
    source.verify_immutable()?;
    let meta = source.metadata();
    let grid = &meta.grid;
    ensure!(
        grid.crs.eq_ignore_ascii_case("EPSG:4326"),
        "HM policy requires EPSG:4326"
    );
    ensure!(
        meta.bands.len() == 1,
        "HM policy requires one registered population band"
    );
    ensure!(
        meta.bands[0].scale == 1. && meta.bands[0].offset == 0.,
        "HM raw population compatibility requires identity scale and offset"
    );
    grid.validate()?;
    let extent = bounds(grid, 0, 0, grid.width, grid.height);
    ensure!(
        extent[0] >= -180. && extent[2] <= 180. && extent[1] >= -85. && extent[3] <= 85.,
        "HM source outside supported lon/lat extent"
    );
    let resolved_index = index_path.map(|p| {
        if p.ends_with(".rsi") {
            p.to_owned()
        } else {
            format!("{}/summary.rsi", p.trim_end_matches('/'))
        }
    });
    let mut index = resolved_index
        .as_deref()
        .map(|p| PersistedIndex::open(p, expected_build_id, read_bytes, cancel))
        .transpose()?;
    if let Some(ix) = &index {
        ix.verify_source(source)?;
        ensure!(
            ix.header().boundary_source == BoundarySource::Original,
            "HM reuse requires original-source summaries"
        );
    } else {
        ensure!(
            expected_build_id.is_none(),
            "HM build identity requires an index"
        );
    }
    let edge = index.as_ref().map_or(256, |i| i.header().tile_edge);
    let plan = coverage::compile_with_budget(
        grid,
        input,
        "EPSG:4326",
        "scanline",
        cancel,
        GEOMETRY_BYTES / 4,
    )?;
    let rings = parse_original(input)?;
    let vertices: usize = rings.iter().map(|r| r.0.len()).sum();
    let ring_reserve = vertices
        .checked_mul(8192)
        .context("HM geometry scratch overflow")?;
    let index_metadata_reserve = if index.is_some() { 8 * 1024 * 1024 } else { 0 };
    let mut tracked = plan
        .bytes()
        .checked_mul(2)
        .and_then(|n| n.checked_add(ring_reserve + 4096 + index_metadata_reserve))
        .context("HM geometry memory overflow")?;
    ensure!(
        tracked <= GEOMETRY_BYTES,
        "HM geometry memory budget exceeded"
    );
    let exact_needed = plan.cells.iter().any(|c| c.fraction < 1e-7) || plan.polygon_area < 1e-6;
    let exact: Option<QRings> = exact_needed.then(|| {
        rings
            .iter()
            .map(|(ring, sign)| {
                (
                    ring.iter()
                        .map(|p| {
                            [
                                BigRational::from_float(p[0]).unwrap(),
                                BigRational::from_float(p[1]).unwrap(),
                            ]
                        })
                        .collect(),
                    *sign,
                )
            })
            .collect()
    });
    let total_area = if let Some(q) = &exact {
        let mut sum = Sum::default();
        for (r, sign) in q {
            sum.add(sign * qarea(r)?);
        }
        sum.value()
    } else {
        area(&rings)
    };
    ensure!(
        total_area.is_finite() && total_area > 0.,
        "HM polygon spherical area unrepresentable"
    );
    let mut work = Work::default();
    let mut clipped_area = Sum::default();
    if let Some(q) = &exact {
        clipped_area.add(qoverlap(q, extent, &mut work, cancel)?);
    } else {
        for (ring, sign) in &rings {
            clipped_area.add(sign * ring_area(&clip_rect(ring, extent, &mut work, cancel)?));
        }
    }
    let footprint = (clipped_area.value() / total_area).clamp(0., 1.);
    ensure!(footprint.is_finite(), "HM footprint is nonfinite");
    let mut selections: BTreeMap<(usize, usize), Selection> = BTreeMap::new();
    for s in &plan.spans {
        check_cancel(cancel)?;
        let mut x = s.start;
        while x < s.end {
            let key = (s.row / edge, x / edge);
            let end = s.end.min((key.1 + 1) * edge);
            tracked = tracked
                .checked_add(256 + 2 * std::mem::size_of::<Span>())
                .context("HM partition overflow")?;
            ensure!(
                tracked <= GEOMETRY_BYTES
                    && (selections.contains_key(&key) || selections.len() < MAX_TILES),
                "HM partition memory/tile budget exceeded"
            );
            selections.entry(key).or_default().spans.push(Span {
                row: s.row,
                start: x,
                end,
            });
            x = end;
        }
    }
    for (n, c) in plan.cells.iter().enumerate() {
        if n % 256 == 0 {
            check_cancel(cancel)?;
        }
        let f = fraction(
            &rings,
            exact.as_ref().filter(|_| c.fraction < 1e-7),
            bounds(grid, c.col, c.row, 1, 1),
            &mut work,
            cancel,
        )?;
        if f == 0. {
            continue;
        }
        let key = (c.row / edge, c.col / edge);
        tracked = tracked
            .checked_add(256 + 2 * std::mem::size_of::<Cell>())
            .context("HM partition overflow")?;
        ensure!(
            tracked <= GEOMETRY_BYTES
                && (selections.contains_key(&key) || selections.len() < MAX_TILES),
            "HM partition memory/tile budget exceeded"
        );
        selections.entry(key).or_default().cells.push(Cell {
            row: c.row,
            col: c.col,
            fraction: f,
        });
    }
    let compile_ms = started.elapsed().as_secs_f64() * 1000.;
    let mut mass = Sum::default();
    let mut selected = Sum::default();
    let mut valid = Sum::default();
    let (mut full, mut boundary, mut windows, mut summarized, mut rejected_summaries) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut read_decode_ms = 0.;
    let mut normalized_bytes = 0usize;
    for (&(ty, tx), selection) in &selections {
        check_cancel(cancel)?;
        let (x, y) = (tx * edge, ty * edge);
        let (w, h) = (edge.min(grid.width - x), edge.min(grid.height - y));
        let count = selection
            .spans
            .iter()
            .map(|s| s.end - s.start)
            .sum::<usize>()
            + selection.cells.len();
        let whole = count == w * h && selection.cells.iter().all(|c| c.fraction == 1.);
        if whole {
            if let Some(ix) = &mut index {
                let values = ix.read_leaf_summary(ty * grid.width.div_ceil(edge) + tx, cancel)?;
                let s = &values[0];
                if s.valid_count == 0 || s.min >= 0. {
                    for part in s.sum.parts() {
                        mass.add(part);
                    }
                    valid.add(s.valid_count as f64);
                    selected.add((w * h) as f64);
                    full += w * h;
                    summarized += 1;
                    continue;
                }
                rejected_summaries += 1;
            }
        }
        let bound = source.read_buffer_bound(w, h, &[0])?;
        ensure!(
            bound <= read_bytes,
            "HM source decode exceeds reader budget"
        );
        ensure!(
            tracked
                .checked_add(bound)
                .and_then(|n| n.checked_add(read_bytes))
                .is_some_and(|n| n <= RESERVATION_BYTES),
            "HM combined reader/geometry budget exceeded"
        );
        ensure!(
            normalized_bytes
                .checked_add(w * h * 9)
                .is_some_and(|n| n <= 2 * 1024 * 1024 * 1024),
            "HM decoded IO budget exceeded before read"
        );
        let (mut raster, metrics) =
            source.read_selected_window_cancellable(x, y, w, h, &[0], read_bytes, cancel)?;
        validate_window(meta, &raster, [x, y, w, h], &[0])?;
        check_cancel(cancel)?;
        let band = &mut raster.bands[0];
        windows += 1;
        read_decode_ms += metrics.read_decode_ms + metrics.normalization_ms;
        normalized_bytes = normalized_bytes
            .checked_add(w * h * 9)
            .context("HM decoded byte overflow")?;
        ensure!(
            normalized_bytes <= 2 * 1024 * 1024 * 1024,
            "HM decoded IO budget exceeded"
        );
        for span in &selection.spans {
            full += span.end - span.start;
            selected.add((span.end - span.start) as f64);
            for x in span.start..span.end {
                if x % 4096 == 0 {
                    check_cancel(cancel)?;
                }
                let i = (span.row - y) * w + x - tx * edge;
                if band.valid[i] && band.values[i] >= 0. {
                    valid.add(1.);
                    mass.add(band.values[i]);
                }
            }
        }
        for cell in &selection.cells {
            if cell.fraction == 1. {
                full += 1;
            } else {
                boundary += 1;
            }
            selected.add(cell.fraction);
            let i = (cell.row - y) * w + cell.col - x;
            if band.valid[i] && band.values[i] >= 0. {
                valid.add(cell.fraction);
                mass.add(band.values[i] * cell.fraction);
            }
        }
    }
    source.verify_immutable()?;
    if let Some(ix) = &index {
        ix.verify_source(source)?;
    }
    ensure!(
        mass.value().is_finite() && selected.value().is_finite() && valid.value().is_finite(),
        "HM accumulation is nonfinite"
    );
    let identity=blake3::hash(&serde_json::to_vec(&json!({"policy":POLICY,"grid":grid,"geometry":input,"population_validity":"finite_masked_nonnegative","snap_to_one":1e-10}))?).to_hex().to_string();
    Ok(
        json!({"mode":POLICY,"policy_identity":identity,"source_id":meta.source_id,"grid":grid,
      "mass":mass.value().max(0.),"fullPixelCount":full,"boundaryPixelCount":boundary,
      "coveredPixelEquivalent":selected.value(),"validPopulationPixelEquivalent":valid.value(),
      "footprintCoverageFraction":footprint,"spherical_area_twice_unit_sphere":total_area,
      "strategy":if index.is_some(){"hm_original_summary_or_raw"}else{"hm_original_direct"},
      "index_build_id":index.as_ref().map(|i|i.header().build_id.clone()),
      "index_access":index.as_ref().map(|i|i.diagnostics()),
      "work":{"windows_read":windows,"summary_records_used":summarized,"negative_summary_fallbacks":rejected_summaries,
        "normalized_bytes":normalized_bytes,"geometry_bound_bytes":tracked,"clip_vertex_visits":work.vertices,
        "boundary_weights":work.weights,"exact_clipping":exact_needed,"result_cache":false},
      "timing_ms":{"compile_and_validate":compile_ms,"read_decode":read_decode_ms,"complete":started.elapsed().as_secs_f64()*1000.},
      "source_access":source.diagnostics()}),
    )
}
