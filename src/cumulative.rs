//! Independently derived fractional value-field boundary integration.
//! See docs/CUMULATIVE_FINDINGS.md for derivation, numerical guards and provenance.
use crate::{aggregate, coverage, model::*};
use anyhow::{Result, ensure};
use geo::Area;
use serde::Serialize;
use serde_json::Value;
use std::{sync::atomic::AtomicBool, time::Instant};

const MAX_SEGMENTS: usize = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    Full,
    Blocked { columns: usize },
}

/// Bounds enclose exact real arithmetic on the supplied binary64 operands.
/// No fast-math or implicit reassociation is allowed.
#[derive(Clone, Copy, Debug, Default)]
struct I {
    lo: f64,
    hi: f64,
}
impl I {
    fn point(x: f64) -> Self {
        Self { lo: x, hi: x }
    }
    fn add(self, b: Self) -> Self {
        if !self.finite() || !b.finite() {
            return Self {
                lo: f64::NEG_INFINITY,
                hi: f64::INFINITY,
            };
        }
        fn endpoint(a: f64, b: f64, lower: bool) -> f64 {
            let s = a + b;
            let v = s - a;
            let error = (a - (s - v)) + (b - v);
            if error == 0. && s.is_finite() {
                s
            } else if lower {
                s.next_down()
            } else {
                s.next_up()
            }
        }
        Self {
            lo: endpoint(self.lo, b.lo, true),
            hi: endpoint(self.hi, b.hi, false),
        }
    }
    fn neg(self) -> Self {
        Self {
            lo: -self.hi,
            hi: -self.lo,
        }
    }
    fn sub(self, b: Self) -> Self {
        self.add(b.neg())
    }
    fn mul(self, b: Self) -> Self {
        if !self.finite() || !b.finite() {
            return Self {
                lo: f64::NEG_INFINITY,
                hi: f64::INFINITY,
            };
        }
        let products = [
            (self.lo, b.lo),
            (self.lo, b.hi),
            (self.hi, b.lo),
            (self.hi, b.hi),
        ];
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for (a, b) in products {
            let p = a * b;
            let exact =
                a == 0. || b == 0. || (p.is_normal() && p.abs() > 1e-290 && a.mul_add(b, -p) == 0.);
            lo = lo.min(if exact { p } else { p.next_down() });
            hi = hi.max(if exact { p } else { p.next_up() });
        }
        Self { lo, hi }
    }
    fn div(self, b: Self) -> Result<Self> {
        ensure!(self.finite() && b.finite(), "nonfinite cumulative division");
        ensure!(b.lo > 0. || b.hi < 0., "uncertain cumulative division");
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for a in [self.lo, self.hi] {
            for d in [b.lo, b.hi] {
                let q = a / d;
                let exact = a == 0.
                    || (q.is_normal()
                        && a.abs() > 1e-290
                        && d.abs() > 1e-290
                        && (-q).mul_add(d, a) == 0.);
                lo = lo.min(if exact { q } else { q.next_down() });
                hi = hi.max(if exact { q } else { q.next_up() });
            }
        }
        Ok(Self { lo, hi })
    }
    fn middle(self) -> f64 {
        self.lo * 0.5 + self.hi * 0.5
    }
    fn radius(self) -> f64 {
        if self.lo == self.hi {
            return 0.;
        }
        let middle = self.middle();
        (middle - self.lo).max(self.hi - middle).next_up()
    }
    fn finite(self) -> bool {
        self.lo.is_finite() && self.hi.is_finite() && self.lo <= self.hi
    }
}

#[derive(Clone, Copy)]
struct Point {
    x: I,
    y: I,
    // Equal identities prove equal exact coordinates, unlike merely overlapping
    // intervals. false denotes submitted world bits; true a grid clip constant.
    x_identity: Option<(bool, u64)>,
    y_identity: Option<(bool, u64)>,
}
impl Point {
    fn delta(start: I, end: I, a: Option<(bool, u64)>, b: Option<(bool, u64)>) -> I {
        if a.is_some() && a == b {
            I::point(0.)
        } else {
            end.sub(start)
        }
    }
    fn lerp(self, other: Self, t: I) -> Self {
        if t.lo == 0. && t.hi == 0. {
            return self;
        }
        if t.lo == 1. && t.hi == 1. {
            return other;
        }
        let same_x = self.x_identity.is_some() && self.x_identity == other.x_identity;
        let same_y = self.y_identity.is_some() && self.y_identity == other.y_identity;
        Self {
            x: if same_x {
                self.x
            } else {
                self.x.add(other.x.sub(self.x).mul(t))
            },
            y: if same_y {
                self.y
            } else {
                self.y.add(other.y.sub(self.y).mul(t))
            },
            x_identity: if same_x { self.x_identity } else { None },
            y_identity: if same_y { self.y_identity } else { None },
        }
    }
}

struct FieldBand {
    values: Vec<I>,
    support: Vec<u32>,
}

pub struct CumulativeField {
    pub grid: Grid,
    pub source_id: String,
    pub band_ids: Vec<usize>,
    pub origin: Origin,
    pub bytes: usize,
    pub preparation_ms: f64,
    fields: Vec<FieldBand>,
}

#[derive(Debug, Serialize)]
pub struct CumulativeBandResult {
    pub band: usize,
    pub fractional_sum: f64,
    pub covered_cell_equivalents: f64,
    pub coverage_weighted_mean: Option<f64>,
    /// Interval radii apply only to successful cumulative evaluation.
    pub sum_error_bound: Option<f64>,
    pub support_error_bound: Option<f64>,
}

#[derive(Default, Debug, Serialize)]
pub struct CumulativeDiagnostics {
    pub strategy: String,
    pub fallback_reason: Option<String>,
    pub boundary_segments: usize,
    pub prefix_reads: usize,
    pub boundary_value_reads: usize,
    pub boundary_mask_reads: usize,
    pub strip_ring_clips: usize,
    pub strict_fallback_cell_visits: usize,
    pub preparation_bytes: usize,
    pub preparation_ms: f64,
    pub query_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct CumulativeResult {
    pub bands: Vec<CumulativeBandResult>,
    pub diagnostics: CumulativeDiagnostics,
}

impl CumulativeField {
    /// Retained allocation estimate; build additionally reserves bounded query scratch.
    pub fn estimate_bytes(r: &Raster, bands: &[usize], origin: Origin) -> Result<usize> {
        r.grid.validate()?;
        ensure!(
            !bands.is_empty() && bands.len() <= 20 && bands.iter().all(|&b| b < r.bands.len()),
            "invalid cumulative band selection"
        );
        ensure!(
            bands
                .iter()
                .enumerate()
                .all(|(i, b)| !bands[..i].contains(b)),
            "duplicate cumulative bands"
        );
        if let Origin::Blocked { columns } = origin {
            ensure!(
                columns > 0 && columns <= 10_000_000,
                "invalid cumulative block width"
            );
        }
        r.grid
            .height
            .checked_mul(r.grid.width + 1)
            .and_then(|n| n.checked_mul(bands.len()))
            .and_then(|n| n.checked_mul(20))
            .and_then(|n| {
                n.checked_add(
                    bands.len() * (std::mem::size_of::<FieldBand>() + std::mem::size_of::<usize>())
                        + std::mem::size_of::<Self>()
                        + r.source_id.len()
                        + r.grid.crs.len(),
                )
            })
            .ok_or_else(|| anyhow::anyhow!("cumulative allocation overflow"))
    }

    pub fn build(r: &Raster, origin: Origin, cancel: &AtomicBool) -> Result<Self> {
        Self::build_selected(r, &(0..r.bands.len()).collect::<Vec<_>>(), origin, cancel)
    }

    pub fn build_selected(
        r: &Raster,
        bands: &[usize],
        origin: Origin,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let started = Instant::now();
        check_cancel(cancel)?;
        r.validate()?;
        ensure!(
            !bands.is_empty() && bands.len() <= 20 && bands.iter().all(|&b| b < r.bands.len()),
            "invalid cumulative band selection"
        );
        ensure!(
            bands
                .iter()
                .enumerate()
                .all(|(i, b)| !bands[..i].contains(b)),
            "duplicate cumulative bands"
        );
        let block = match origin {
            Origin::Full => r.grid.width,
            Origin::Blocked { columns } => {
                ensure!(
                    columns > 0 && columns <= 10_000_000,
                    "invalid cumulative block width"
                );
                columns
            }
        };
        // At each block start the preceding column's end prefix occupies the
        // ordinary gridline slot. Starts are implicit zero, so no extra slots.
        let entries = r
            .grid
            .height
            .checked_mul(r.grid.width + 1)
            .ok_or_else(|| anyhow::anyhow!("cumulative dimensions overflow"))?;
        let bytes = Self::estimate_bytes(r, bands, origin)?;
        ensure!(
            bytes
                .checked_add(r.bytes())
                .and_then(
                    |n| n.checked_add(MAX_SEGMENTS * std::mem::size_of::<I>() + 16 * 1024 * 1024)
                )
                .is_some_and(|n| n <= MAX_BYTES),
            "cumulative index, raster and scratch reservation exceed 2 GiB; select fewer bands"
        );
        let mut fields = Vec::with_capacity(bands.len());
        for &bi in bands {
            let b = &r.bands[bi];
            let mut values = vec![I::default(); entries];
            let mut support = vec![0; entries];
            for row in 0..r.grid.height {
                check_cancel(cancel)?;
                let mut sum = I::default();
                let mut count = 0u32;
                for col in 0..r.grid.width {
                    if col % 4096 == 0 {
                        check_cancel(cancel)?;
                    }
                    if col % block == 0 {
                        sum = I::default();
                        count = 0;
                    }
                    let cell = row * r.grid.width + col;
                    if b.valid[cell] {
                        sum = sum.add(I::point(b.values[cell]));
                        count += 1;
                    }
                    ensure!(sum.finite(), "nonfinite cumulative preparation");
                    let pos = row * (r.grid.width + 1) + col + 1;
                    values[pos] = sum;
                    support[pos] = count;
                }
            }
            fields.push(FieldBand { values, support });
        }
        Ok(Self {
            grid: r.grid.clone(),
            source_id: r.source_id.clone(),
            band_ids: bands.to_vec(),
            origin,
            bytes,
            preparation_ms: started.elapsed().as_secs_f64() * 1000.,
            fields,
        })
    }

    pub fn query(
        &self,
        r: &Raster,
        input: &Value,
        cancel: &AtomicBool,
    ) -> Result<CumulativeResult> {
        self.query_selected(r, input, &self.band_ids, true, cancel)
    }

    pub fn query_selected(
        &self,
        r: &Raster,
        input: &Value,
        bands: &[usize],
        needs_sum: bool,
        cancel: &AtomicBool,
    ) -> Result<CumulativeResult> {
        let started = Instant::now();
        check_cancel(cancel)?;
        ensure!(
            r.grid == self.grid && r.source_id == self.source_id,
            "stale cumulative source/grid identity"
        );
        ensure!(
            !bands.is_empty()
                && bands.len() <= 20
                && bands
                    .iter()
                    .enumerate()
                    .all(|(i, b)| !bands[..i].contains(b)),
            "invalid cumulative query bands"
        );
        let selection = bands
            .iter()
            .map(|b| {
                self.band_ids.iter().position(|p| p == b).ok_or_else(|| {
                    anyhow::anyhow!("requested band was not prepared in cumulative index")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (geometry, _) = coverage::geometry(input, &self.grid, cancel)?;
        let mut diagnostics = CumulativeDiagnostics {
            preparation_bytes: self.bytes,
            preparation_ms: self.preparation_ms,
            ..Default::default()
        };
        let area = geometry.unsigned_area();
        let perimeter: f64 = geometry
            .0
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors().iter()))
            .flat_map(|r| r.lines())
            .map(|e| (e.end.x - e.start.x).hypot(e.end.y - e.start.y))
            .sum();
        let attempt = if area > 0. && (area < 1e-4 || area < perimeter * 1e-7) {
            Err(anyhow::anyhow!(
                "small/thin support requires strict exact-rational coverage"
            ))
        } else {
            self.integrate(
                r,
                input,
                &geometry,
                &selection,
                needs_sum,
                cancel,
                &mut diagnostics,
            )
        };
        let bands = match attempt {
            Ok(bands) => {
                diagnostics.strategy = match self.origin {
                    Origin::Full => "cumulative_full",
                    Origin::Blocked { .. } => "cumulative_blocked",
                }
                .to_owned();
                bands
            }
            Err(error) => {
                check_cancel(cancel)?;
                diagnostics.strategy = "strict_scanline_fallback".to_owned();
                diagnostics.fallback_reason = Some(error.to_string());
                let plan = coverage::compile_polygon(
                    &self.grid,
                    input,
                    &self.grid.crs,
                    "scanline",
                    cancel,
                )?;
                diagnostics.strict_fallback_cell_visits =
                    plan.intersecting.saturating_mul(bands.len());
                let mut options = aggregate::Options::default();
                options.bands = bands.to_vec();
                options.statistics = Some(if needs_sum {
                    vec!["sum".into(), "support".into(), "mean".into()]
                } else {
                    vec!["support".into()]
                });
                aggregate::measure(r, &plan, &options, None, cancel)?
                    .into_iter()
                    .map(|b| CumulativeBandResult {
                        band: b.band,
                        fractional_sum: b.fractional_sum,
                        covered_cell_equivalents: b.covered_cell_equivalents,
                        coverage_weighted_mean: if needs_sum {
                            b.coverage_weighted_mean
                        } else {
                            None
                        },
                        sum_error_bound: None,
                        support_error_bound: None,
                    })
                    .collect()
            }
        };
        diagnostics.query_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(CumulativeResult { bands, diagnostics })
    }

    fn integrate(
        &self,
        r: &Raster,
        input: &Value,
        geometry: &geo::MultiPolygon<f64>,
        selection: &[usize],
        needs_sum: bool,
        cancel: &AtomicBool,
        d: &mut CumulativeDiagnostics,
    ) -> Result<Vec<CumulativeBandResult>> {
        let mut sums = vec![I::default(); selection.len()];
        let mut supports = sums.clone();
        let raw_polygons: Vec<&Value> = if input["type"] == "Polygon" {
            vec![&input["coordinates"]]
        } else {
            input["coordinates"].as_array().unwrap().iter().collect()
        };
        let block = match self.origin {
            Origin::Full => self.grid.width,
            Origin::Blocked { columns } => columns,
        };
        for (polygon, raw) in geometry.0.iter().zip(
            raw_polygons
                .into_iter()
                .filter(|p| p.as_array().is_some_and(|a| !a.is_empty())),
        ) {
            for (ring_index, (ring, raw_ring)) in std::iter::once(polygon.exterior())
                .chain(polygon.interiors().iter())
                .zip(raw.as_array().unwrap())
                .enumerate()
            {
                let sign = geo::Polygon::new(ring.clone(), vec![])
                    .signed_area()
                    .signum()
                    * if ring_index == 0 { 1. } else { -1. };
                let points = read_points(raw_ring, &self.grid)?;
                let min_x = points
                    .iter()
                    .map(|p| p.x.lo)
                    .fold(f64::INFINITY, f64::min)
                    .max(0.)
                    .min(self.grid.width as f64);
                let max_x = points
                    .iter()
                    .map(|p| p.x.hi)
                    .fold(f64::NEG_INFINITY, f64::max)
                    .max(0.)
                    .min(self.grid.width as f64);
                if max_x <= min_x {
                    continue;
                }
                let first = (min_x as usize / block) * block;
                for origin in (first..max_x.ceil() as usize).step_by(block) {
                    check_cancel(cancel)?;
                    let end = (origin + block).min(self.grid.width);
                    let clipped = if matches!(self.origin, Origin::Full) {
                        points.clone()
                    } else {
                        d.strip_ring_clips += 1;
                        ensure!(
                            d.strip_ring_clips.saturating_mul(points.len()) <= 100_000_000,
                            "cumulative strip clipping work budget exceeded"
                        );
                        let clipped = clip_x(&points, origin as f64, true)?;
                        clip_x(&clipped, end as f64, false)?
                    };
                    if clipped.len() < 3 {
                        continue;
                    }
                    let mut previous = *clipped.last().unwrap();
                    for &point in &clipped {
                        self.edge(
                            r,
                            previous,
                            point,
                            origin,
                            end,
                            sign,
                            selection,
                            needs_sum,
                            &mut sums,
                            &mut supports,
                            d,
                            cancel,
                        )?;
                        previous = point;
                    }
                }
            }
        }
        sums.into_iter()
            .zip(supports)
            .enumerate()
            .map(|(index, (s, n))| {
                ensure!(
                    s.finite() && n.finite(),
                    "nonfinite cumulative query arithmetic"
                );
                let sum = s.middle();
                let support = n.middle();
                // These use |sum|, not sum(abs(f*v)), so cannot loosen SPEC.
                ensure!(
                    s.radius() <= (1e-8 + 1e-10 * sum.abs()) * 0.25,
                    "cumulative sum cancellation bound exceeds tolerance"
                );
                ensure!(
                    n.radius() <= (1e-8 + 1e-10 * support.abs()) * 0.25,
                    "cumulative support cancellation bound exceeds tolerance"
                );
                ensure!(n.lo >= 0., "uncertain empty/negative cumulative support");
                let mean = if n.hi == 0. || !needs_sum {
                    None
                } else {
                    ensure!(n.lo > 0., "uncertain positive cumulative support");
                    let m = s.div(n)?;
                    ensure!(
                        m.finite() && m.radius() <= (1e-8 + 1e-10 * m.middle().abs()) * 0.25,
                        "cumulative mean conditioning exceeds tolerance"
                    );
                    Some(m.middle())
                };
                Ok(CumulativeBandResult {
                    band: self.band_ids[selection[index]],
                    fractional_sum: sum,
                    covered_cell_equivalents: support,
                    coverage_weighted_mean: mean,
                    sum_error_bound: Some(s.radius()),
                    support_error_bound: Some(n.radius()),
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn edge(
        &self,
        r: &Raster,
        a: Point,
        b: Point,
        origin: usize,
        end: usize,
        sign: f64,
        selection: &[usize],
        needs_sum: bool,
        sums: &mut [I],
        supports: &mut [I],
        d: &mut CumulativeDiagnostics,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let dy = Point::delta(a.y, b.y, a.y_identity, b.y_identity);
        if dy.lo == 0. && dy.hi == 0. {
            return Ok(());
        }
        let mut events = vec![I::point(0.), I::point(1.)];
        for (start, finish, identity_a, identity_b, low, high) in [
            (a.x, b.x, a.x_identity, b.x_identity, origin, end),
            (a.y, b.y, a.y_identity, b.y_identity, 0, self.grid.height),
        ] {
            let delta = Point::delta(start, finish, identity_a, identity_b);
            if delta.lo == 0. && delta.hi == 0. {
                continue;
            }
            ensure!(
                delta.lo > 0. || delta.hi < 0.,
                "uncertain cumulative edge direction"
            );
            let first = start.middle().min(finish.middle()).floor().max(low as f64) as usize;
            let last = start
                .middle()
                .max(finish.middle())
                .ceil()
                .min(high as f64)
                .max(0.) as usize;
            ensure!(
                last.saturating_sub(first) <= MAX_SEGMENTS,
                "cumulative edge event budget exceeded"
            );
            for boundary in first..=last {
                let t = I::point(boundary as f64).sub(start).div(delta)?;
                if t.hi < 0. || t.lo > 1. {
                    continue;
                }
                if t.lo == 0. && t.hi == 0. || t.lo == 1. && t.hi == 1. {
                    continue;
                }
                ensure!(t.lo > 0. && t.hi < 1., "uncertain endpoint/grid crossing");
                ensure!(
                    events.len() < MAX_SEGMENTS,
                    "cumulative event allocation budget exceeded"
                );
                if events.len() == events.capacity() {
                    let capacity = events.capacity().saturating_mul(2).min(MAX_SEGMENTS);
                    events.try_reserve_exact(capacity - events.len())?;
                }
                events.push(t);
            }
        }
        events.sort_unstable_by(|a, b| a.middle().total_cmp(&b.middle()));
        events.dedup_by(|a, b| a.lo == a.hi && b.lo == b.hi && a.lo == b.lo);
        for pair in events.windows(2) {
            ensure!(
                pair[0].hi < pair[1].lo,
                "uncertain coincident grid crossings"
            );
            d.boundary_segments += 1;
            ensure!(
                d.boundary_segments <= MAX_SEGMENTS,
                "cumulative segment budget exceeded"
            );
            if d.boundary_segments % 1024 == 0 {
                check_cancel(cancel)?;
            }
            let middle_t = (pair[0].middle() + pair[1].middle()) * 0.5;
            ensure!(
                middle_t > pair[0].hi && middle_t < pair[1].lo,
                "unresolved cumulative segment interior"
            );
            let midpoint = a.lerp(b, I::point(middle_t));
            let y = midpoint.y.middle();
            if y < 0. || y >= self.grid.height as f64 {
                ensure!(
                    midpoint.y.hi <= 0. || midpoint.y.lo >= self.grid.height as f64,
                    "uncertain row extent classification"
                );
                continue;
            }
            let row = y.floor() as usize;
            ensure!(
                midpoint.y.lo >= row as f64 && midpoint.y.hi <= (row + 1) as f64,
                "uncertain row classification"
            );
            let x = midpoint.x.middle();
            if x <= origin as f64 {
                ensure!(
                    midpoint.x.hi <= origin as f64,
                    "uncertain left field boundary"
                );
                continue;
            }
            let col = x.floor().max(origin as f64).min(end as f64) as usize;
            ensure!(
                midpoint.x.lo >= col as f64 && (col == end || midpoint.x.hi <= (col + 1) as f64),
                "uncertain column classification"
            );
            let ax = a.lerp(b, pair[0]).x;
            let bx = a.lerp(b, pair[1]).x;
            let offset = if col == end {
                I::point(0.)
            } else {
                ax.add(bx).mul(I::point(0.5)).sub(I::point(col as f64))
            };
            let height = dy.mul(pair[1].sub(pair[0])).mul(I::point(sign));
            for (index, &prepared_index) in selection.iter().enumerate() {
                let bi = self.band_ids[prepared_index];
                let prefix = row * (self.grid.width + 1) + col;
                let (p, n) = if col == origin {
                    (I::default(), I::default())
                } else {
                    (
                        if needs_sum {
                            self.fields[prepared_index].values[prefix]
                        } else {
                            I::default()
                        },
                        I::point(self.fields[prepared_index].support[prefix] as f64),
                    )
                };
                d.prefix_reads += 1 + usize::from(needs_sum);
                let (value, valid) = if col < end {
                    d.boundary_mask_reads += 1;
                    d.boundary_value_reads += usize::from(needs_sum);
                    let cell = row * self.grid.width + col;
                    if r.bands[bi].valid[cell] {
                        (
                            if needs_sum {
                                r.bands[bi].values[cell]
                            } else {
                                0.
                            },
                            1.,
                        )
                    } else {
                        (0., 0.)
                    }
                } else {
                    (0., 0.)
                };
                if needs_sum {
                    sums[index] = sums[index].add(p.add(offset.mul(I::point(value))).mul(height));
                }
                supports[index] =
                    supports[index].add(n.add(offset.mul(I::point(valid))).mul(height));
            }
        }
        Ok(())
    }
}

fn read_points(ring: &Value, grid: &Grid) -> Result<Vec<Point>> {
    ring.as_array()
        .unwrap()
        .iter()
        .map(|p| {
            Ok(Point {
                x_identity: Some((false, p[0].as_f64().unwrap().to_bits())),
                y_identity: Some((false, p[1].as_f64().unwrap().to_bits())),
                x: I::point(p[0].as_f64().unwrap())
                    .sub(I::point(grid.transform[0]))
                    .div(I::point(grid.transform[1]))?,
                y: I::point(p[1].as_f64().unwrap())
                    .sub(I::point(grid.transform[3]))
                    .div(I::point(grid.transform[5]))?,
            })
        })
        .collect()
}

// Convex half-plane clipping can produce a boundary walk with opposite bridge
// traversals for disconnected pieces; their signed integrals cancel exactly.
fn clip_x(points: &[Point], bound: f64, greater: bool) -> Result<Vec<Point>> {
    if points.is_empty() {
        return Ok(vec![]);
    }
    let inside = |p: Point| -> Result<bool> {
        ensure!(
            p.x.hi <= bound || p.x.lo >= bound,
            "uncertain strip-side classification"
        );
        Ok(if greater {
            p.x.lo >= bound
        } else {
            p.x.hi <= bound
        })
    };
    let mut out = Vec::with_capacity(points.len() + 2);
    let mut previous = *points.last().unwrap();
    for &point in points {
        if inside(previous)? != inside(point)? {
            let t = I::point(bound)
                .sub(previous.x)
                .div(point.x.sub(previous.x))?;
            let mut intersection = previous.lerp(point, t);
            intersection.x = I::point(bound);
            intersection.x_identity = Some((true, bound.to_bits()));
            out.push(intersection);
        }
        if inside(point)? {
            out.push(point);
        }
        previous = point;
    }
    Ok(out)
}

#[cfg(test)]
mod architecture_cumulative_interval_tests {
    use super::I;
    use num_rational::BigRational;
    fn encloses(interval: I, exact: BigRational) {
        if interval.finite() {
            assert!(BigRational::from_float(interval.lo).unwrap() <= exact);
            assert!(exact <= BigRational::from_float(interval.hi).unwrap());
        }
    }
    #[test]
    fn architecture_cumulative_interval_arithmetic_encloses_exact_binary64() {
        let values = [
            0.,
            1.,
            -1.,
            0.1,
            -0.25,
            1e16,
            -1e16,
            1e150,
            1e-290,
            1e-300,
            f64::from_bits(1),
            -f64::from_bits(1),
        ];
        for a in values {
            for b in values {
                let ra = BigRational::from_float(a).unwrap();
                let rb = BigRational::from_float(b).unwrap();
                encloses(I::point(a).add(I::point(b)), &ra + &rb);
                encloses(I::point(a).sub(I::point(b)), &ra - &rb);
                encloses(I::point(a).mul(I::point(b)), &ra * &rb);
                if b != 0. {
                    encloses(I::point(a).div(I::point(b)).unwrap(), &ra / &rb);
                }
            }
        }
    }
}
