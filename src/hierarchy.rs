//! Flat native-grid aggregate hierarchy. Query traversal precedes any cell plan.
//! The rectangle certificate examines every exterior AND hole boundary; checking
//! the four corners alone is insufficient (a hole may be wholly inside a node).
use crate::{
    aggregate::{BandResult, Histogram, Options},
    coverage::{self, Cell},
    model::*,
};
use anyhow::{Result, ensure};
use geo::{Area, BoundingRect, Contains, Coord, Intersects, Line, MultiPolygon, Point, Rect};
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, mem::size_of, sync::atomic::AtomicBool, time::Instant};

/// Half-open pixel bounds are [x0,y0,x1,y1]; the certificate tests their closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Relation {
    Outside,
    Inside,
    Boundary,
}

/// One validated geometry, shared by hierarchical and flat persistent traversal.
/// Pixel coordinates retain the original raster affine; leaf grids are never
/// re-anchored, which would change the exact binary64 small-support contract.
pub(crate) struct GeometryPredicate {
    grid: Grid,
    input: Value,
    poly: MultiPolygon<f64>,
    edges: Vec<Line<f64>>,
    vertices: usize,
    area: f64,
    bounds: [usize; 4],
    precise: bool,
    guard: f64,
}
impl GeometryPredicate {
    pub(crate) fn new(grid: &Grid, input: &Value, crs: &str, cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        grid.validate()?;
        ensure!(
            crs == grid.crs,
            "query CRS must exactly match source CRS; reprojection is not implicit"
        );
        let (poly, vertices) = coverage::geometry(input, grid, cancel)?;
        let mut area = poly.unsigned_area();
        ensure!(area.is_finite(), "nonfinite polygon area");
        let mut edges: Vec<_> = poly
            .0
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors().iter()))
            .flat_map(|r| r.lines())
            .collect();
        let perimeter: f64 = edges
            .iter()
            .map(|l| (l.end.x - l.start.x).hypot(l.end.y - l.start.y))
            .sum();
        let precise = area > 0. && (area < 1e-4 || area < perimeter * 1e-7);
        let mut bounds = poly.bounding_rect().map_or([0; 4], |r| {
            [
                r.min().x.floor().max(0.).min(grid.width as f64) as usize,
                r.min().y.floor().max(0.).min(grid.height as f64) as usize,
                r.max().x.ceil().max(0.).min(grid.width as f64) as usize,
                r.max().y.ceil().max(0.).min(grid.height as f64) as usize,
            ]
        });
        if precise {
            bounds = [
                bounds[0].saturating_sub(1),
                bounds[1].saturating_sub(1),
                (bounds[2] + 1).min(grid.width),
                (bounds[3] + 1).min(grid.height),
            ];
            let candidates = (bounds[2] - bounds[0]).saturating_mul(bounds[3] - bounds[1]);
            ensure!(
                candidates <= 100_000 && candidates.saturating_mul(vertices) <= 1_000_000,
                "exact precision fallback work budget exceeded"
            );
            // Obtain the exact affine-derived area without enumerating any cell.
            area = crate::precise::coverage(grid, input, [0; 4], vertices, MAX_BYTES, cancel)?.2;
        }
        // Subtraction followed by division contributes only a few ulps of the
        // normalized coordinate. Inflate by 16 ulps and an absolute floor before
        // certification; uncertain near-boundary nodes are refined, never rounded
        // into an interior node. geo's segment predicate uses robust orientation.
        let magnitude = edges
            .iter()
            .flat_map(|e| [e.start.x, e.start.y, e.end.x, e.end.y])
            .map(f64::abs)
            .fold(1., f64::max);
        let guard = 16. * f64::EPSILON * magnitude + 1e-12;
        edges.sort_unstable_by(|a, b| a.start.y.min(a.end.y).total_cmp(&b.start.y.min(b.end.y)));
        let input = serde_json::json!({"type":input["type"],"coordinates":input["coordinates"]});
        Ok(Self {
            grid: grid.clone(),
            input,
            poly,
            edges,
            vertices,
            area,
            bounds,
            precise,
            guard,
        })
    }
    pub(crate) fn polygon_area(&self) -> f64 {
        self.area
    }
    pub(crate) fn vertices(&self) -> usize {
        self.vertices
    }
    pub(crate) fn bounds(&self) -> [usize; 4] {
        self.bounds
    }
    pub(crate) fn is_precise(&self) -> bool {
        self.precise
    }
    pub(crate) fn classify(&self, bounds: [usize; 4]) -> Relation {
        self.classify_counted(bounds).0
    }
    fn classify_counted(&self, b: [usize; 4]) -> (Relation, usize, usize) {
        if b[0] == b[2]
            || b[1] == b[3]
            || self.edges.is_empty()
            || b[2] <= self.bounds[0]
            || b[0] >= self.bounds[2]
            || b[3] <= self.bounds[1]
            || b[1] >= self.bounds[3]
        {
            return (Relation::Outside, 0, 0);
        }
        if self.precise {
            return (Relation::Boundary, 0, 0);
        }
        let r = Rect::new(
            Coord {
                x: b[0] as f64 - self.guard,
                y: b[1] as f64 - self.guard,
            },
            Coord {
                x: b[2] as f64 + self.guard,
                y: b[3] as f64 + self.guard,
            },
        );
        let mut tested = 0;
        let mut work = 0;
        for edge in &self.edges {
            work += 1;
            if edge.start.y.min(edge.end.y) > r.max().y {
                break;
            }
            if edge.start.y.max(edge.end.y) < r.min().y
                || edge.start.x.max(edge.end.x) < r.min().x
                || edge.start.x.min(edge.end.x) > r.max().x
            {
                continue;
            }
            tested += 1;
            // This detects segments entirely contained in the rectangle too.
            // In particular every wholly contained hole forces refinement.
            if r.intersects(edge) {
                return (Relation::Boundary, tested, work);
            }
        }
        let center = Point::new((b[0] + b[2]) as f64 * 0.5, (b[1] + b[3]) as f64 * 0.5);
        (
            if self.poly.contains(&center) {
                Relation::Inside
            } else {
                Relation::Outside
            },
            tested,
            work + self.edges.len(),
        )
    }
    /// Fractional coverage for a bounded ambiguous leaf; no complete raster Plan.
    /// Global coordinates and the existing clipper/precision fallback are reused.
    pub(crate) fn boundary_cells(&self, b: [usize; 4], cancel: &AtomicBool) -> Result<Vec<Cell>> {
        ensure!(
            b[0] <= b[2] && b[1] <= b[3] && b[2] <= self.grid.width && b[3] <= self.grid.height,
            "boundary window outside raster"
        );
        let b = [
            b[0].max(self.bounds[0]),
            b[1].max(self.bounds[1]),
            b[2].min(self.bounds[2]),
            b[3].min(self.bounds[3]),
        ];
        if b[0] >= b[2] || b[1] >= b[3] {
            return Ok(Vec::new());
        }
        let candidate_cells = (b[2] - b[0])
            .checked_mul(b[3] - b[1])
            .ok_or_else(|| anyhow::anyhow!("boundary size overflow"))?;
        ensure!(
            candidate_cells <= MAX_ENTRIES
                && (b[3] - b[1]).saturating_mul(self.vertices) <= 100_000_000,
            "boundary coverage work budget exceeded"
        );
        if self.precise {
            return Ok(crate::precise::coverage(
                &self.grid,
                &self.input,
                [b[0], b[2], b[1], b[3]],
                self.vertices,
                MAX_BYTES,
                cancel,
            )?
            .0);
        }
        coverage::compile_window(
            &self.grid,
            &self.input,
            &self.poly,
            self.vertices,
            b,
            cancel,
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    bounds: [usize; 4],
    children: [usize; 4],
    child_count: u8,
    depth: u8,
}
#[derive(Clone, Copy, Debug)]
struct Summary {
    sum: Sum,
    count: usize,
    min: f64,
    max: f64,
}
impl Default for Summary {
    fn default() -> Self {
        Self {
            sum: Sum::default(),
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}
impl Summary {
    fn merge(&mut self, other: Self) {
        self.sum.merge(other.sum);
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }
}

/// Immutable source ownership is enforced by Session; callers of this lower
/// level API must not mutate the raster after preparing it.
#[derive(Debug)]
pub struct Hierarchy {
    pub source_id: String,
    pub grid: Grid,
    pub bytes: usize,
    pub preparation_ms: f64,
    pub leaf_size: usize,
    band_count: usize,
    nodes: Vec<Node>,
    summaries: Vec<Summary>,
}
#[derive(Default, Serialize)]
pub struct Diagnostics {
    pub nodes_visited: usize,
    pub nodes_inside: usize,
    pub nodes_outside: usize,
    pub nodes_boundary: usize,
    pub boundary_leaves: usize,
    pub max_visited_depth: usize,
    pub segment_rectangle_tests: usize,
    pub geometry_edge_work_upper_bound: usize,
    pub node_metadata_bytes_touched: usize,
    pub summary_bytes_touched: usize,
    pub summarized_cells_per_band: usize,
    pub raw_band_cell_visits: usize,
    pub avoided_raw_band_cell_visits: usize,
    pub boundary_candidate_cells: usize,
    pub boundary_positive_cells: usize,
    pub index_bytes: usize,
    pub index_nodes: usize,
    pub leaf_size: usize,
    pub geometry_validation_ms: f64,
    pub query_ms: f64,
    pub precise_boundary_fallback: bool,
    pub full_resolution_plan_created: bool,
    pub auxiliary_raw_fallback: bool,
}
#[derive(Serialize)]
pub struct QueryResult {
    pub bands: Vec<BandResult>,
    pub diagnostics: Diagnostics,
}

fn child_bounds(b: [usize; 4]) -> Vec<[usize; 4]> {
    let xm = b[0] + (b[2] - b[0]) / 2;
    let ym = b[1] + (b[3] - b[1]) / 2;
    let xs = if xm == b[0] {
        vec![(b[0], b[2])]
    } else {
        vec![(b[0], xm), (xm, b[2])]
    };
    let ys = if ym == b[1] {
        vec![(b[1], b[3])]
    } else {
        vec![(b[1], ym), (ym, b[3])]
    };
    ys.iter()
        .flat_map(|&(y0, y1)| xs.iter().map(move |&(x0, x1)| [x0, y0, x1, y1]))
        .collect()
}
fn count_nodes(b: [usize; 4], leaf: usize) -> usize {
    // Memoize repeated quadrant dimensions so a rejected one-cell-leaf request
    // never traverses millions of hypothetical nodes merely to estimate memory.
    fn count(w: usize, h: usize, leaf: usize, memo: &mut HashMap<(usize, usize), usize>) -> usize {
        if w <= leaf && h <= leaf {
            return 1;
        }
        if let Some(&n) = memo.get(&(w, h)) {
            return n;
        }
        let n = 1 + child_bounds([0, 0, w, h])
            .iter()
            .map(|b| count(b[2] - b[0], b[3] - b[1], leaf, memo))
            .sum::<usize>();
        memo.insert((w, h), n);
        n
    }
    count(b[2] - b[0], b[3] - b[1], leaf, &mut HashMap::new())
}
impl Hierarchy {
    pub fn estimate_bytes(r: &Raster, leaf_size: usize) -> Result<usize> {
        ensure!(
            (1..=4096).contains(&leaf_size),
            "hierarchy leaf_size must be 1..4096"
        );
        let nodes = count_nodes([0, 0, r.grid.width, r.grid.height], leaf_size);
        nodes
            .checked_mul(size_of::<Node>() + r.bands.len() * size_of::<Summary>())
            .and_then(|n| n.checked_add(size_of::<Self>() + r.source_id.len() + r.grid.crs.len()))
            .ok_or_else(|| anyhow::anyhow!("hierarchy size overflow"))
    }
    pub fn build(r: &Raster, leaf_size: usize, cancel: &AtomicBool) -> Result<Self> {
        let start = Instant::now();
        r.validate()?;
        check_cancel(cancel)?;
        let bytes = Self::estimate_bytes(r, leaf_size)?;
        ensure!(
            r.bytes().saturating_add(bytes) <= MAX_BYTES,
            "hierarchy memory budget exceeded"
        );
        let count = count_nodes([0, 0, r.grid.width, r.grid.height], leaf_size);
        let mut h = Self {
            source_id: r.source_id.clone(),
            grid: r.grid.clone(),
            bytes,
            preparation_ms: 0.,
            leaf_size,
            band_count: r.bands.len(),
            nodes: Vec::with_capacity(count),
            summaries: Vec::with_capacity(count * r.bands.len()),
        };
        h.build_node(r, [0, 0, r.grid.width, r.grid.height], 0, cancel)?;
        h.preparation_ms = start.elapsed().as_secs_f64() * 1000.;
        Ok(h)
    }
    fn build_node(
        &mut self,
        r: &Raster,
        b: [usize; 4],
        depth: u8,
        cancel: &AtomicBool,
    ) -> Result<usize> {
        check_cancel(cancel)?;
        let id = self.nodes.len();
        self.nodes.push(Node {
            bounds: b,
            children: [0; 4],
            child_count: 0,
            depth,
        });
        self.summaries
            .resize(self.summaries.len() + self.band_count, Summary::default());
        if b[2] - b[0] <= self.leaf_size && b[3] - b[1] <= self.leaf_size {
            for (bi, band) in r.bands.iter().enumerate() {
                let mut s = Summary::default();
                for row in b[1]..b[3] {
                    check_cancel(cancel)?;
                    for col in b[0]..b[2] {
                        let i = row * r.grid.width + col;
                        if band.valid[i] {
                            let v = band.values[i];
                            s.sum.add(v);
                            s.count += 1;
                            s.min = s.min.min(v);
                            s.max = s.max.max(v);
                        }
                    }
                }
                ensure!(s.sum.value().is_finite(), "nonfinite hierarchy arithmetic");
                self.summaries[id * self.band_count + bi] = s;
            }
        } else {
            for child in child_bounds(b) {
                let ci = self.build_node(r, child, depth + 1, cancel)?;
                let n = &mut self.nodes[id];
                n.children[n.child_count as usize] = ci;
                n.child_count += 1;
                for bi in 0..self.band_count {
                    let summary = self.summaries[ci * self.band_count + bi];
                    self.summaries[id * self.band_count + bi].merge(summary);
                }
            }
            ensure!(
                self.summaries[id * self.band_count..(id + 1) * self.band_count]
                    .iter()
                    .all(|s| s.sum.value().is_finite()),
                "nonfinite hierarchy arithmetic"
            );
        }
        Ok(id)
    }
    pub fn query(
        &self,
        r: &Raster,
        input: &Value,
        crs: &str,
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<QueryResult> {
        let begin = Instant::now();
        ensure!(
            r.source_id == self.source_id
                && r.grid == self.grid
                && r.bands.len() == self.band_count,
            "stale hierarchy source/grid identity"
        );
        options.validate(r.bands.len())?;
        if options.has_new_reducers() {
            let plan = crate::coverage::compile_polygon(&r.grid, input, crs, "scanline", cancel)?;
            let bands = crate::aggregate::measure(r, &plan, options, None, cancel)?;
            return Ok(QueryResult {
                bands,
                diagnostics: Diagnostics {
                    index_bytes: self.bytes,
                    index_nodes: self.nodes.len(),
                    leaf_size: self.leaf_size,
                    full_resolution_plan_created: true,
                    auxiliary_raw_fallback: true,
                    raw_band_cell_visits: plan
                        .intersecting
                        .saturating_mul(options.selected_bands(r.bands.len()).len()),
                    query_ms: begin.elapsed().as_secs_f64() * 1000.,
                    ..Default::default()
                },
            });
        }
        // Reject before allocating the parsed geometry, including the maximum
        // permitted vertex scratch. The later guard also includes leaf cells.
        ensure!(
            r.bytes()
                .saturating_add(self.bytes)
                .saturating_add(MAX_VERTICES * 8192 + 65536)
                <= MAX_BYTES,
            "hierarchy query memory budget exceeded"
        );
        let geometry = GeometryPredicate::new(&r.grid, input, crs, cancel)?;
        let bands = options.selected_bands(r.bands.len());
        let mut acc: Vec<_> = bands.iter().map(|_| Acc::new(options)).collect();
        let auxiliary = options.histogram_edges.is_some() || options.weight_band.is_some();
        let mut d = Diagnostics {
            index_bytes: self.bytes,
            index_nodes: self.nodes.len(),
            leaf_size: self.leaf_size,
            geometry_validation_ms: begin.elapsed().as_secs_f64() * 1000.,
            precise_boundary_fallback: geometry.is_precise(),
            auxiliary_raw_fallback: auxiliary,
            ..Default::default()
        };
        // Reserve the maximum boundary leaf and geometry scratch before query.
        // No stack entry owns cell arrays; at most one leaf is decoded at a time.
        let candidate = if geometry.precise {
            (geometry.bounds[2] - geometry.bounds[0])
                .saturating_mul(geometry.bounds[3] - geometry.bounds[1])
        } else {
            self.leaf_size
                .min(r.grid.width)
                .saturating_mul(self.leaf_size.min(r.grid.height))
        };
        let scratch = candidate
            .saturating_mul(size_of::<Cell>() * 2)
            .saturating_add(geometry.vertices.saturating_mul(8192))
            .saturating_add(65536);
        ensure!(
            r.bytes().saturating_add(self.bytes).saturating_add(scratch) <= MAX_BYTES,
            "hierarchy query memory budget exceeded"
        );
        let mut selected = Sum::default();
        let mut intersecting = 0usize;
        let mut stack = vec![0usize];
        while let Some(id) = stack.pop() {
            check_cancel(cancel)?;
            let n = self.nodes[id];
            d.nodes_visited += 1;
            d.node_metadata_bytes_touched += size_of::<Node>();
            d.max_visited_depth = d.max_visited_depth.max(n.depth as usize);
            let (relation, tests, work) = geometry.classify_counted(n.bounds);
            d.segment_rectangle_tests += tests;
            d.geometry_edge_work_upper_bound += work;
            ensure!(
                d.geometry_edge_work_upper_bound <= 100_000_000,
                "hierarchy geometry work budget exceeded"
            );
            match relation {
                Relation::Outside => d.nodes_outside += 1,
                Relation::Inside => {
                    d.nodes_inside += 1;
                    let cells = (n.bounds[2] - n.bounds[0]) * (n.bounds[3] - n.bounds[1]);
                    selected.add(cells as f64);
                    intersecting += cells;
                    d.summarized_cells_per_band += cells;
                    d.summary_bytes_touched += bands.len() * size_of::<Summary>();
                    for (ai, &bi) in bands.iter().enumerate() {
                        acc[ai].summary(self.summaries[id * self.band_count + bi], options);
                        if auxiliary {
                            for row in n.bounds[1]..n.bounds[3] {
                                check_cancel(cancel)?;
                                for col in n.bounds[0]..n.bounds[2] {
                                    acc[ai].auxiliary(r, bi, row * r.grid.width + col, 1., options);
                                }
                            }
                            d.raw_band_cell_visits += cells;
                        } else {
                            d.avoided_raw_band_cell_visits += cells;
                        }
                    }
                }
                Relation::Boundary => {
                    d.nodes_boundary += 1;
                    if n.child_count > 0 && !geometry.precise {
                        stack.extend(n.children[..n.child_count as usize].iter().rev());
                    } else {
                        d.boundary_leaves += 1;
                        let b = if geometry.precise {
                            geometry.bounds
                        } else {
                            n.bounds
                        };
                        d.boundary_candidate_cells += (b[2] - b[0]) * (b[3] - b[1]);
                        let cells = geometry.boundary_cells(b, cancel)?;
                        d.boundary_positive_cells += cells.len();
                        for c in cells {
                            selected.add(c.fraction);
                            intersecting += 1;
                            for (ai, &bi) in bands.iter().enumerate() {
                                acc[ai].cell(
                                    r,
                                    bi,
                                    c.row * r.grid.width + c.col,
                                    c.fraction,
                                    options,
                                );
                                d.raw_band_cell_visits += 1;
                            }
                        }
                    }
                }
            }
        }
        let selected = selected.value();
        ensure!(
            selected.is_finite() && selected <= geometry.area + 1e-8 + geometry.area.abs() * 1e-10,
            "hierarchy coverage exceeds polygon area"
        );
        let results = bands
            .iter()
            .zip(acc)
            .map(|(&bi, a)| a.finish(r, bi, selected, geometry.area, intersecting, options))
            .collect::<Result<Vec<_>>>()?;
        d.query_ms = begin.elapsed().as_secs_f64() * 1000.;
        Ok(QueryResult {
            bands: results,
            diagnostics: d,
        })
    }
}

struct Acc {
    sum: Sum,
    valid: Sum,
    count: usize,
    min: f64,
    max: f64,
    hist: Vec<Sum>,
    under: Sum,
    over: Sum,
    weighted: Sum,
    weights: Sum,
    weighted_valid: Sum,
}
impl Acc {
    fn new(options: &Options) -> Self {
        Self {
            sum: Sum::default(),
            valid: Sum::default(),
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            hist: vec![Sum::default(); options.histogram_edges.as_ref().map_or(0, |e| e.len() - 1)],
            under: Sum::default(),
            over: Sum::default(),
            weighted: Sum::default(),
            weights: Sum::default(),
            weighted_valid: Sum::default(),
        }
    }
    fn summary(&mut self, s: Summary, o: &Options) {
        if o.needs_sum() {
            self.sum.merge(s.sum);
        }
        self.valid.add(s.count as f64);
        self.count += s.count;
        if o.needs_min() {
            self.min = self.min.min(s.min);
        }
        if o.needs_max() {
            self.max = self.max.max(s.max);
        }
    }
    fn cell(&mut self, r: &Raster, bi: usize, i: usize, f: f64, o: &Options) {
        let b = &r.bands[bi];
        if !b.valid[i] {
            return;
        }
        if o.needs_sum() {
            self.sum.add(b.values[i] * f);
        }
        self.valid.add(f);
        self.count += 1;
        if o.needs_min() {
            self.min = self.min.min(b.values[i]);
        }
        if o.needs_max() {
            self.max = self.max.max(b.values[i]);
        }
        self.auxiliary(r, bi, i, f, o);
    }
    fn auxiliary(&mut self, r: &Raster, bi: usize, i: usize, f: f64, o: &Options) {
        let b = &r.bands[bi];
        if !b.valid[i] {
            return;
        }
        let v = b.values[i];
        if let Some(wi) = o.weight_band {
            let w = &r.bands[wi];
            if w.valid[i] && w.values[i] >= 0. {
                if o.needs_weighted_sum() {
                    self.weighted.add(v * w.values[i] * f);
                }
                if o.needs_weight_sum() {
                    self.weights.add(w.values[i] * f);
                }
                self.weighted_valid.add(f);
            }
        }
        if let Some(e) = &o.histogram_edges {
            if v < e[0] {
                self.under.add(f);
            } else if v > *e.last().unwrap() {
                self.over.add(f);
            } else {
                let bin = e
                    .partition_point(|edge| *edge <= v)
                    .saturating_sub(1)
                    .min(e.len() - 2);
                self.hist[bin].add(f);
            }
        }
    }
    fn finish(
        self,
        r: &Raster,
        bi: usize,
        selected: f64,
        area: f64,
        intersecting: usize,
        o: &Options,
    ) -> Result<BandResult> {
        let sum = self.sum.value();
        let valid = self.valid.value();
        let ws = self.weighted.value();
        let ww = self.weights.value();
        ensure!(
            [sum, valid, ws, ww].iter().all(|v| v.is_finite()),
            "nonfinite hierarchy query arithmetic"
        );
        ensure!(
            (valid == 0. || (sum / valid).is_finite()) && (ww == 0. || (ws / ww).is_finite()),
            "nonfinite mean arithmetic"
        );
        let missing = (selected - valid).max(0.);
        let outside = (area - selected).max(0.);
        let status = if area == 0. {
            "empty"
        } else if selected == 0. {
            "outside"
        } else if valid == 0. {
            "no_valid_data"
        } else if missing > 1e-9 || outside > 1e-9 {
            "partial"
        } else {
            "ok"
        };
        Ok(BandResult {
            extensions: Default::default(),
            band: bi,
            fractional_sum: sum,
            covered_cell_equivalents: valid,
            selected_cell_equivalents: selected,
            missing_cell_equivalents: missing,
            outside_cell_equivalents: outside,
            intersecting_cell_count: intersecting,
            valid_cell_count: self.count,
            coverage_weighted_mean: if valid > 0. { Some(sum / valid) } else { None },
            min: if self.count > 0 && o.needs_min() {
                Some(self.min)
            } else {
                None
            },
            max: if self.count > 0 && o.needs_max() {
                Some(self.max)
            } else {
                None
            },
            status: status.to_owned(),
            unit: r.bands[bi].unit.clone(),
            histogram: o.histogram_edges.as_ref().map(|e| Histogram {
                edges: e.clone(),
                covered_cell_equivalents: self.hist.iter().map(|s| s.value()).collect(),
                underflow: self.under.value(),
                overflow: self.over.value(),
            }),
            weighted_sum: o.weight_band.map(|_| ws),
            weight_sum: o.weight_band.map(|_| ww),
            weighted_mean: if o.weight_band.is_some() && ww > 0. {
                Some(ws / ww)
            } else {
                None
            },
            weighted_valid_cell_equivalents: o.weight_band.map(|_| self.weighted_valid.value()),
        })
    }
}

#[cfg(test)]
mod architecture_hierarchy_certificate_tests {
    use super::*;
    use serde_json::json;
    fn predicate(rings: Value) -> GeometryPredicate {
        let grid = Grid {
            width: 16,
            height: 16,
            transform: [0., 1., 0., 16., 0., -1.],
            crs: "LOCAL".into(),
        };
        GeometryPredicate::new(
            &grid,
            &json!({"type":"Polygon","coordinates":rings}),
            "LOCAL",
            &AtomicBool::new(false),
        )
        .unwrap()
    }
    #[test]
    fn contained_hole_and_contained_island_force_boundary_without_corner_crossings() {
        let p = predicate(json!([
            [[0., 16.], [16., 16.], [16., 0.], [0., 0.], [0., 16.]],
            [[3., 13.], [4., 13.], [4., 12.], [3., 12.], [3., 13.]]
        ]));
        assert_eq!(p.classify([1, 1, 8, 8]), Relation::Boundary);
        assert_eq!(p.classify([5, 5, 8, 8]), Relation::Inside);
        let island = predicate(json!([[
            [3., 13.],
            [4., 13.],
            [4., 12.],
            [3., 12.],
            [3., 13.]
        ]]));
        assert_eq!(island.classify([1, 1, 8, 8]), Relation::Boundary);
        assert_eq!(island.classify([9, 9, 12, 12]), Relation::Outside);
    }
    #[test]
    fn edge_and_corner_contact_never_certify_whole_rectangle() {
        let p = predicate(json!([[
            [1., 15.],
            [9., 15.],
            [9., 7.],
            [1., 7.],
            [1., 15.]
        ]]));
        assert_eq!(p.classify([1, 1, 4, 4]), Relation::Boundary);
        assert_eq!(p.classify([0, 0, 2, 2]), Relation::Boundary);
        assert_eq!(p.classify([2, 2, 4, 4]), Relation::Inside);
    }
}
