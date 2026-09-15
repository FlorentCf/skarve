use crate::model::*;
use anyhow::{Result, bail, ensure};
use geo::{
    Area, BoundingRect, Coord, Intersects, LineString, MultiPolygon, Polygon, RemoveRepeatedPoints,
    Validation,
};
use serde::Serialize;
use serde_json::Value;
use std::{sync::atomic::AtomicBool, time::Instant};

#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct Cell {
    pub row: usize,
    pub col: usize,
    pub fraction: f64,
}
#[derive(Debug, Clone)]
pub struct Plan {
    pub grid: Grid,
    pub grid_id: String,
    pub identity: String,
    pub spans: Vec<Span>,
    pub cells: Vec<Cell>,
    pub selected: f64,
    pub polygon_area: f64,
    pub intersecting: usize,
    pub strategy: String,
    pub validation_ms: f64,
    pub compilation_ms: f64,
}
impl Plan {
    pub fn bytes(&self) -> usize {
        self.spans.len() * std::mem::size_of::<Span>()
            + self.cells.len() * std::mem::size_of::<Cell>()
    }
    pub fn diagnostics(&self) -> Value {
        serde_json::json!({"identity":self.identity,"grid_id":self.grid_id,"strategy":self.strategy,"full_spans":self.spans.len(),"boundary_cells":self.cells.len(),"bytes":self.bytes(),"selected_cell_equivalents":self.selected,"polygon_cell_equivalents":self.polygon_area,"intersecting_cell_count":self.intersecting,"cache_hit":false})
    }
    pub fn debug_cells(&self) -> Result<Vec<Cell>> {
        ensure!(
            self.intersecting <= 100_000,
            "debug output limited to 100000 cells"
        );
        let mut cells = self.cells.clone();
        for s in &self.spans {
            for col in s.start..s.end {
                cells.push(Cell {
                    row: s.row,
                    col,
                    fraction: 1.,
                });
            }
        }
        cells.sort_by_key(|c| (c.row, c.col));
        Ok(cells)
    }
}

pub(crate) type Ring = Vec<Coord<f64>>;
fn parse_ring(v: &Value, grid: &Grid, count: &mut usize) -> Result<LineString<f64>> {
    let values = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("ring must be array"))?;
    ensure!(values.len() >= 4, "closed ring requires >=4 coordinates");
    *count += values.len();
    ensure!(*count <= MAX_VERTICES, "vertex budget exceeded");
    let mut coords = Vec::with_capacity(values.len());
    for p in values {
        let xy = p
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("coordinate must be array"))?;
        ensure!(xy.len() == 2, "only 2D coordinates supported");
        let x = xy[0]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("nonfinite coordinate"))?;
        let y = xy[1]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("nonfinite coordinate"))?;
        ensure!(x.is_finite() && y.is_finite(), "nonfinite coordinate");
        if grid.crs.eq_ignore_ascii_case("EPSG:4326") {
            ensure!(
                (-180.0..=180.0).contains(&x) && (-85.0..=85.0).contains(&y),
                "geographic polar/longitude bounds unsupported"
            );
        }
        let px = (x - grid.transform[0]) / grid.transform[1];
        let py = (y - grid.transform[3]) / grid.transform[5];
        ensure!(
            px.is_finite() && py.is_finite() && px.abs() <= 1e9 && py.abs() <= 1e9,
            "coordinate precision budget exceeded"
        );
        coords.push(Coord { x: px, y: py });
    }
    ensure!(
        coords.first() == coords.last(),
        "rings must be explicitly closed"
    );
    if grid.crs.eq_ignore_ascii_case("EPSG:4326") {
        for pair in coords.windows(2) {
            ensure!(
                ((pair[1].x - pair[0].x) * grid.transform[1]).abs() <= 180.,
                "antimeridian edge unsupported"
            );
        }
    }
    Ok(LineString(coords))
}
fn parse_polygon(v: &Value, grid: &Grid, count: &mut usize) -> Result<Option<Polygon<f64>>> {
    let rings = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("polygon coordinates must be array"))?;
    ensure!(rings.len() <= 129, "hole-count budget exceeded");
    if rings.is_empty() {
        return Ok(None);
    }
    let exterior = parse_ring(&rings[0], grid, count)?;
    let holes = rings[1..]
        .iter()
        .map(|r| parse_ring(r, grid, count))
        .collect::<Result<Vec<_>>>()?;
    for ring in std::iter::once(&exterior).chain(holes.iter()) {
        ensure!(
            Polygon::new(ring.clone(), vec![]).signed_area() != 0.,
            "invalid polygon: degenerate zero-area ring"
        );
    }
    Ok(Some(Polygon::new(exterior, holes)))
}
// Exact same segment predicate as geo's ring validator, with a conservative
// bounding-box sweep to avoid invoking it on disjoint edges. Holes/components
// retain geo's full topological relation checks. No polygon repair occurs.
fn validate_single_ring(ring: &LineString<f64>, cancel: &AtomicBool) -> Result<()> {
    ensure!(
        ring.remove_repeated_points().0.len() >= 4,
        "invalid polygon: too few distinct ring points"
    );
    let mut edges: Vec<_> = ring
        .lines()
        .map(|line| {
            (
                line.start.x.min(line.end.x),
                line.start.x.max(line.end.x),
                line.start.y.min(line.end.y),
                line.start.y.max(line.end.y),
                line,
            )
        })
        .collect();
    edges.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (i, a) in edges.iter().enumerate() {
        check_cancel(cancel)?;
        for b in &edges[i + 1..] {
            if b.0 > a.1 {
                break;
            }
            if a.3 < b.2 || b.3 < a.2 {
                continue;
            }
            if a.4.start == b.4.end || a.4.end == b.4.start {
                continue;
            }
            ensure!(
                !a.4.intersects(&b.4),
                "invalid polygon: ring self-intersection"
            );
        }
    }
    Ok(())
}

pub(crate) fn geometry(
    v: &Value,
    grid: &Grid,
    cancel: &AtomicBool,
) -> Result<(MultiPolygon<f64>, usize)> {
    let mut count = 0;
    let mut polys = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        Some("Polygon") => {
            if let Some(p) = parse_polygon(&v["coordinates"], grid, &mut count)? {
                polys.push(p);
            }
        }
        Some("MultiPolygon") => {
            let parts = v["coordinates"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("multipolygon coordinates must be array"))?;
            ensure!(parts.len() <= 128, "component-count budget exceeded");
            for p in parts {
                if let Some(p) = parse_polygon(p, grid, &mut count)? {
                    polys.push(p);
                }
            }
        }
        _ => bail!("only GeoJSON Polygon/MultiPolygon supported"),
    }
    let multi = MultiPolygon(polys);
    ensure!(
        multi.0.iter().map(|p| p.interiors().len()).sum::<usize>() <= 128,
        "total hole-count budget exceeded"
    );
    // First error only: collecting pairwise errors can itself become unbounded.
    if multi.0.len() == 1 && multi.0[0].interiors().is_empty() {
        validate_single_ring(multi.0[0].exterior(), cancel)?;
    } else if let Err(error) = multi.check_validation() {
        bail!("invalid polygon or overlapping components: {error}");
    }
    Ok((multi, count))
}

// Independently authored Sutherland-Hodgman half-plane clipping, applied to
// polygon rings. Opposite bridge traversals cancel in the shoelace integral.
fn clip(ring: &[Coord<f64>], axis: usize, bound: f64, keep_greater: bool) -> Ring {
    if ring.is_empty() {
        return Vec::new();
    }
    let coord = |p: Coord<f64>| if axis == 0 { p.x } else { p.y };
    let inside = |p: Coord<f64>| {
        if keep_greater {
            coord(p) >= bound
        } else {
            coord(p) <= bound
        }
    };
    let mut out = Vec::with_capacity(ring.len() + 2);
    let mut a = *ring.last().unwrap();
    for &b in ring {
        if inside(a) != inside(b) {
            let t = (bound - coord(a)) / (coord(b) - coord(a));
            let mut p = Coord {
                x: a.x + t * (b.x - a.x),
                y: a.y + t * (b.y - a.y),
            };
            if axis == 0 {
                p.x = bound
            } else {
                p.y = bound
            };
            out.push(p);
        }
        if inside(b) {
            out.push(b)
        }
        a = b;
    }
    out
}
pub(crate) fn strip(ring: &[Coord<f64>], row: usize) -> Ring {
    clip(&clip(ring, 1, row as f64, true), 1, (row + 1) as f64, false)
}
fn area(ring: &[Coord<f64>], _x: f64, _y: f64) -> f64 {
    if ring.len() < 3 {
        return 0.;
    }
    let (x, y) = (ring[0].x, ring[0].y);
    let mut s = Sum::default();
    let mut a = *ring.last().unwrap();
    for &b in ring {
        s.add((a.x - x) * (b.y - y) - (b.x - x) * (a.y - y));
        a = b;
    }
    s.value().abs() * 0.5
}
pub(crate) fn fraction(strips: &[(Ring, f64)], row: usize, col: usize) -> Result<f64> {
    let mut result = Sum::default();
    for (ring, sign) in strips {
        let cell = clip(&clip(ring, 0, col as f64, true), 0, (col + 1) as f64, false);
        result.add(sign * area(&cell, col as f64, row as f64));
    }
    let f = result.value();
    ensure!(
        f.is_finite() && (-1e-9..=1. + 1e-9).contains(&f),
        "coverage arithmetic outside tolerance: {f}"
    );
    Ok(f.clamp(0., 1.))
}

#[derive(Clone, Copy)]
struct Edge {
    a: Coord<f64>,
    b: Coord<f64>,
    factor: f64,
}
#[derive(Clone, Copy)]
struct RowEdge {
    left: f64,
    right: f64,
    dy: f64,
}
struct RowIntegral {
    edges: Vec<RowEdge>,
    suffix: Vec<Sum>,
}
impl RowIntegral {
    fn new(mut edges: Vec<RowEdge>) -> Self {
        edges.sort_by(|a, b| a.left.total_cmp(&b.left));
        let mut suffix = vec![Sum::default(); edges.len() + 1];
        for i in (0..edges.len()).rev() {
            suffix[i] = suffix[i + 1];
            suffix[i].add(edges[i].dy);
        }
        Self { edges, suffix }
    }
    fn fraction(&self, col: usize) -> f64 {
        // Green's theorem: integral clamp(x-col,0,1) dy around a
        // row-clipped ring equals its area within this unit cell. Horizontal
        // clip bridges have dy=0. For a linear edge the clamped integral is
        // the sum of one trapezoid and a constant tail. Orientation and holes
        // are accounted for in signed dy; no cell polygon is allocated.
        let left = col as f64;
        let right = left + 1.;
        let split = self.edges.partition_point(|e| e.left < right);
        let mut sum = self.suffix[split];
        for e in &self.edges[..split] {
            if e.right <= left {
                continue;
            }
            let a = e.left - left;
            let b = e.right - left;
            let width = e.right - e.left;
            let integral = if width == 0. {
                a.clamp(0., 1.)
            } else {
                let enter = (-a / width).clamp(0., 1.);
                let leave = ((1. - a) / width).clamp(0., 1.);
                (a.clamp(0., 1.) + b.clamp(0., 1.)) * 0.5 * (leave - enter) + (1. - leave)
            };
            sum.add(e.dy * integral);
        }
        sum.value()
    }
}
fn merged(mut intervals: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    intervals.sort_unstable();
    let mut result: Vec<(usize, usize)> = Vec::new();
    for (a, b) in intervals {
        if a >= b {
            continue;
        }
        if let Some(last) = result.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        result.push((a, b));
    }
    result
}

pub fn compile_polygon(
    grid: &Grid,
    input: &Value,
    crs: &str,
    strategy: &str,
    cancel: &AtomicBool,
) -> Result<Plan> {
    compile_with_budget(grid, input, crs, strategy, cancel, MAX_BYTES)
}
pub fn compile_with_budget(
    grid: &Grid,
    input: &Value,
    crs: &str,
    strategy: &str,
    cancel: &AtomicBool,
    max_bytes: usize,
) -> Result<Plan> {
    let begin = Instant::now();
    check_cancel(cancel)?;
    grid.validate()?;
    ensure!(
        crs == grid.crs,
        "query CRS must exactly match source CRS; reprojection is not implicit"
    );
    ensure!(
        matches!(strategy, "scanline" | "direct"),
        "unknown coverage strategy"
    );
    let (poly, vertices) = geometry(input, grid, cancel)?;
    compile_validated(
        grid, input, &poly, vertices, strategy, cancel, max_bytes, None, begin,
    )
}

/// Reuse the ordinary scanline kernel inside a bounded native-grid leaf. Input
/// and topology were validated once by GeometryPredicate. All coordinates and
/// cell IDs retain the original affine, including exact small-support fallback.
pub(crate) fn compile_window(
    grid: &Grid,
    input: &Value,
    poly: &MultiPolygon<f64>,
    vertices: usize,
    bounds: [usize; 4],
    cancel: &AtomicBool,
) -> Result<Vec<Cell>> {
    let plan = compile_validated(
        grid,
        input,
        poly,
        vertices,
        "scanline",
        cancel,
        MAX_BYTES,
        Some(bounds),
        Instant::now(),
    )?;
    let mut cells = plan.cells;
    cells.reserve(plan.intersecting.saturating_sub(cells.len()));
    for span in plan.spans {
        for col in span.start..span.end {
            cells.push(Cell {
                row: span.row,
                col,
                fraction: 1.,
            });
        }
    }
    Ok(cells)
}

#[allow(clippy::too_many_arguments)]
fn compile_validated(
    grid: &Grid,
    input: &Value,
    poly: &MultiPolygon<f64>,
    vertices: usize,
    strategy: &str,
    cancel: &AtomicBool,
    max_bytes: usize,
    window: Option<[usize; 4]>,
    begin: Instant,
) -> Result<Plan> {
    let polygon_area = poly.unsigned_area();
    ensure!(polygon_area.is_finite(), "nonfinite polygon area");
    let grid_id = grid.identity();
    let mut hash = blake3::Hasher::new();
    hash.update(grid_id.as_bytes());
    hash.update(b"native_grid_planar:v1");
    if window.is_none() {
        hash.update(serde_json::to_string(input)?.as_bytes());
    }
    let mut plan = Plan {
        grid: grid.clone(),
        grid_id,
        identity: hash.finalize().to_hex().to_string(),
        spans: vec![],
        cells: vec![],
        selected: 0.,
        polygon_area,
        intersecting: 0,
        strategy: strategy.to_string(),
        validation_ms: begin.elapsed().as_secs_f64() * 1000.,
        compilation_ms: 0.,
    };
    let compile_start = Instant::now();
    let Some(bbox) = poly.bounding_rect() else {
        return Ok(plan);
    };
    let w = window.unwrap_or([0, 0, grid.width, grid.height]);
    let lo = (bbox.min().y.floor().max(0.).min(grid.height as f64) as usize).max(w[1]);
    let hi = (bbox.max().y.ceil().max(0.).min(grid.height as f64) as usize).min(w[3]);
    if window.is_some() && lo >= hi {
        return Ok(plan);
    }
    ensure!(
        (hi - lo)
            .checked_mul(vertices)
            .is_some_and(|n| n <= 100_000_000),
        "geometry work budget exceeded"
    );
    let xmin = (bbox.min().x.floor().max(0.).min(grid.width as f64) as usize).max(w[0]);
    let xmax = (bbox.max().x.ceil().max(0.).min(grid.width as f64) as usize).min(w[2]);
    if window.is_some() && xmin >= xmax {
        return Ok(plan);
    }
    let perimeter: f64 = poly
        .0
        .iter()
        .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors().iter()))
        .flat_map(|r| r.lines())
        .map(|e| (e.end.x - e.start.x).hypot(e.end.y - e.start.y))
        .sum();
    if polygon_area > 0. && (polygon_area < 1e-4 || polygon_area < perimeter * 1e-7) {
        // Small support can make a mean ill-conditioned even when f64 area
        // errors are far below the normal absolute coverage tolerance.
        // Expand the rounded bbox by one cell; exact arithmetic decides which
        // cells truly have positive area. Work and memory remain bounded.
        let bounds = [
            xmin.saturating_sub(1).max(w[0]),
            (xmax + 1).min(w[2]),
            lo.saturating_sub(1).max(w[1]),
            (hi + 1).min(w[3]),
        ];
        let (cells, selected, area) =
            crate::precise::coverage(grid, input, bounds, vertices, max_bytes, cancel)?;
        plan.intersecting = cells.len();
        plan.cells = cells;
        plan.selected = selected;
        plan.polygon_area = area;
        ensure!(
            area > 0. && selected <= area + 1e-8 + area.abs() * 1e-10,
            "exact coverage exceeds polygon area"
        );
        plan.strategy = "exact_rational_small_support".to_owned();
        plan.compilation_ms = compile_start.elapsed().as_secs_f64() * 1000.;
        return Ok(plan);
    }
    if strategy == "direct" {
        ensure!(
            (hi - lo)
                .checked_mul(xmax - xmin)
                .is_some_and(|n| n <= MAX_ENTRIES),
            "direct coverage cell budget exceeded"
        );
    }
    let mut rings: Vec<(Ring, f64)> = Vec::new();
    for p in &poly.0 {
        rings.push((p.exterior().0.clone(), 1.));
        for r in p.interiors() {
            rings.push((r.0.clone(), -1.));
        }
    }
    let mut edges = Vec::with_capacity(vertices);
    for (ring, sign) in &rings {
        let signed = Polygon::new(LineString(ring.clone()), vec![]).signed_area();
        let factor = if signed < 0. { -*sign } else { *sign };
        edges.extend(ring.windows(2).map(|e| Edge {
            a: e[0],
            b: e[1],
            factor,
        }));
    }
    edges.sort_by(|a, b| a.a.y.min(a.b.y).total_cmp(&b.a.y.min(b.b.y)));
    let mut active: Vec<Edge> = Vec::new();
    let mut next_edge = 0;
    let mut selected = Sum::default();
    for row in lo..hi {
        check_cancel(cancel)?;
        // Lazily retain the independent clipper for direct extraction and for
        // near-zero/full or out-of-tolerance integrals. No epsilon discards
        // slivers: ambiguous fractions are recomputed by the original method.
        let mut strips: Option<Vec<(Ring, f64)>> = None;
        let mut boundary = Vec::new();
        let mut xs = Vec::new();
        let mut row_edges = Vec::new();
        let center = row as f64 + 0.5;
        if strategy == "direct" {
            boundary.push((xmin, xmax));
        } else {
            active.retain(|e| e.a.y.max(e.b.y) >= row as f64);
            while next_edge < edges.len()
                && edges[next_edge].a.y.min(edges[next_edge].b.y) <= (row + 1) as f64
            {
                let e = edges[next_edge];
                if e.a.y.max(e.b.y) >= row as f64 {
                    active.push(e);
                }
                next_edge += 1;
            }
            for edge in &active {
                let (a, b) = (edge.a, edge.b);
                if (a.y > center) != (b.y > center) {
                    xs.push(a.x + (center - a.y) * (b.x - a.x) / (b.y - a.y));
                }
                let (u, v) = if a.y == b.y {
                    (a.x, b.x)
                } else {
                    let low = (row as f64).max(a.y.min(b.y));
                    let high = ((row + 1) as f64).min(a.y.max(b.y));
                    (
                        a.x + (low - a.y) * (b.x - a.x) / (b.y - a.y),
                        a.x + (high - a.y) * (b.x - a.x) / (b.y - a.y),
                    )
                };
                let l = u.min(v).floor().max(0.).min(grid.width as f64) as usize;
                let h = (u.max(v).floor() + 1.).max(0.).min(grid.width as f64) as usize;
                boundary.push((l.max(xmin), h.min(xmax)));
                if a.y != b.y {
                    let low = (row as f64).max(a.y.min(b.y));
                    let high = ((row + 1) as f64).min(a.y.max(b.y));
                    let dy = (high - low) * if b.y > a.y { edge.factor } else { -edge.factor };
                    if dy != 0. {
                        row_edges.push(RowEdge {
                            left: u.min(v),
                            right: u.max(v),
                            dy,
                        });
                    }
                }
            }
        }
        let integral = RowIntegral::new(row_edges);
        let boundary = merged(boundary);
        let boundary_n: usize = boundary.iter().map(|(a, b)| b - a).sum();
        ensure!(
            plan.cells.len() + plan.spans.len() + boundary_n <= MAX_ENTRIES.min(max_bytes / 48),
            "coverage plan budget exceeded"
        );
        for &(a, b) in &boundary {
            for col in a..b {
                if col % 1024 == 0 {
                    check_cancel(cancel)?;
                }
                let fast = integral.fraction(col);
                let f = if strategy == "direct"
                    || !fast.is_finite()
                    || !(1e-10..=1. - 1e-10).contains(&fast)
                {
                    let ss = strips.get_or_insert_with(|| {
                        rings
                            .iter()
                            .map(|(r, s)| (strip(r, row), *s))
                            .filter(|(r, _)| !r.is_empty())
                            .collect()
                    });
                    fraction(ss, row, col)?
                } else {
                    fast
                };
                if f > 0. {
                    selected.add(f);
                    plan.intersecting += 1;
                    plan.cells.push(Cell {
                        row,
                        col,
                        fraction: f,
                    });
                }
            }
        }
        if strategy == "scanline" {
            xs.sort_by(f64::total_cmp);
            ensure!(xs.len() % 2 == 0, "unpaired scanline intersections");
            for pair in xs.chunks_exact(2) {
                let mut a =
                    ((pair[0] - 0.5).ceil().max(0.).min(grid.width as f64) as usize).max(xmin);
                let b = ((pair[1] - 0.5).ceil().max(0.).min(grid.width as f64) as usize).min(xmax);
                for &(c, d) in &boundary {
                    if d <= a {
                        continue;
                    }
                    if c >= b {
                        break;
                    }
                    if c > a {
                        plan.spans.push(Span {
                            row,
                            start: a,
                            end: c.min(b),
                        });
                    }
                    a = a.max(d);
                    if a >= b {
                        break;
                    }
                }
                if a < b {
                    plan.spans.push(Span {
                        row,
                        start: a,
                        end: b,
                    });
                }
            }
        }
    }
    ensure!(
        plan.cells.len() + plan.spans.len() <= MAX_ENTRIES.min(max_bytes / 48),
        "coverage plan budget exceeded"
    );
    for s in &plan.spans {
        selected.add((s.end - s.start) as f64);
        plan.intersecting += s.end - s.start;
    }
    ensure!(
        plan.intersecting <= MAX_CELLS,
        "selected-cell work budget exceeded"
    );
    plan.selected = selected.value();
    ensure!(
        plan.selected <= polygon_area + 1e-8 + polygon_area.abs() * 1e-10,
        "coverage exceeds polygon area"
    );
    plan.compilation_ms = compile_start.elapsed().as_secs_f64() * 1000.;
    Ok(plan)
}
