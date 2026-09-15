use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    batch::{BorrowedSource, Job, JobSpec},
    model::{Band, Grid, Raster, Sum},
    persistent::TileSummary,
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
    stored_summary::{StoredSummarySource, SummaryLayout, validated_state},
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    sync::atomic::AtomicBool,
};

struct Synthetic {
    meta: RasterMetadata,
    layout: SummaryLayout,
    summaries: bool,
    reject_interior: bool,
    reads: RefCell<Vec<(usize, usize, Vec<usize>)>>,
    summary_reads: Cell<usize>,
}
impl Synthetic {
    fn new(bands: usize, summaries: bool, reject_interior: bool) -> Self {
        let grid = Grid {
            width: 100,
            height: 100,
            transform: [0., 1., 0., 100., 0., -1.],
            crs: "LOCAL".into(),
        };
        let layout = SummaryLayout::new(&grid, 32, true).unwrap();
        Self {
            meta: RasterMetadata {
                grid,
                source_id: "synthetic-summary-source".into(),
                bands: (0..bands)
                    .map(|_| BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: None,
                        block_size: (32, 32),
                    })
                    .collect(),
            },
            layout,
            summaries,
            reject_interior,
            reads: RefCell::new(Vec::new()),
            summary_reads: Cell::new(0),
        }
    }
    fn sample(b: usize, x: usize, y: usize) -> (f64, bool) {
        (
            (b + 1) as f64 * 1000. + (x as f64 - y as f64) * 0.25,
            (x + 3 * y + b) % 17 != 0,
        )
    }
}
impl WindowSource for Synthetic {
    fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn stored_summaries(&self) -> Option<&dyn StoredSummarySource> {
        self.summaries.then_some(self)
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        raster_engine::model::check_cancel(cancel)?;
        ensure!(
            bands.len() <= 20 && w * h * bands.len() * 9 <= max,
            "synthetic read exceeded group budget"
        );
        ensure!(
            !(self.reject_interior && x <= 32 && y <= 32 && x + w >= 64 && y + h >= 64),
            "protected raw interior"
        );
        self.reads.borrow_mut().push((x, y, bands.to_vec()));
        let mut grid = self.meta.grid.clone();
        grid.width = w;
        grid.height = h;
        grid.transform[0] += x as f64;
        grid.transform[3] -= y as f64;
        let data = bands
            .iter()
            .map(|&b| {
                let samples = (y..y + h)
                    .flat_map(|yy| (x..x + w).map(move |xx| Self::sample(b, xx, yy)))
                    .collect::<Vec<_>>();
                Band {
                    values: samples.iter().map(|s| s.0).collect(),
                    valid: samples.iter().map(|s| s.1).collect(),
                    unit: None,
                }
            })
            .collect();
        Ok((
            Raster {
                grid,
                bands: data,
                source_id: self.meta.source_id.clone(),
            },
            ReadMetrics::default(),
        ))
    }
}
impl StoredSummarySource for Synthetic {
    fn summary_layout(&self) -> &SummaryLayout {
        &self.layout
    }
    fn summary_read_buffer_bound(&self, bands: &[usize]) -> Result<usize> {
        Ok(bands.len() * 128)
    }
    fn read_summary(
        &self,
        record: usize,
        bands: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Vec<TileSummary>> {
        raster_engine::model::check_cancel(cancel)?;
        ensure!(bands.len() <= 20, "summary group overflow");
        self.summary_reads.set(self.summary_reads.get() + 1);
        let [x0, y0, x1, y1] = self.layout.bounds(&self.meta.grid, record)?;
        bands
            .iter()
            .map(|&b| {
                let (mut sum, mut n, mut min, mut max) =
                    (Sum::default(), 0u64, f64::INFINITY, f64::NEG_INFINITY);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let (v, valid) = Self::sample(b, x, y);
                        if valid {
                            sum.add(v);
                            n += 1;
                            min = min.min(v);
                            max = max.max(v);
                        }
                    }
                }
                validated_state(
                    sum.parts(),
                    n,
                    if n == 0 { 0. } else { min },
                    if n == 0 { 0. } else { max },
                    (x1 - x0) * (y1 - y0),
                )
            })
            .collect()
    }
}
fn polygon(a: f64, b: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[a,a],[b,a],[b,b],[a,b],[a,a]]]})
}
fn options() -> Options {
    Options {
        statistics: Some(
            ["sum", "support", "mean", "min", "max", "count"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Options::default()
    }
}
fn single(s: &Synthetic, g: &Value, o: &Options) -> Result<Value> {
    streaming::measure_cached_source(
        s,
        g,
        "LOCAL",
        o,
        &AtomicBool::new(false),
        &mut TileCache::default(),
        0.,
    )
}
fn compare(a: &Value, b: &Value) {
    assert_eq!(a.as_array().unwrap().len(), b.as_array().unwrap().len());
    for (a, b) in a.as_array().unwrap().iter().zip(b.as_array().unwrap()) {
        for k in ["band", "valid_cell_count"] {
            assert_eq!(a[k], b[k], "{k}");
        }
        for k in [
            "fractional_sum",
            "covered_cell_equivalents",
            "coverage_weighted_mean",
            "min",
            "max",
        ] {
            if a[k].is_null() {
                assert_eq!(a[k], b[k]);
            } else {
                let (x, y) = (a[k].as_f64().unwrap(), b[k].as_f64().unwrap());
                assert!(
                    (x - y).abs() <= 1e-8 + 1e-10 * x.abs().max(y.abs()),
                    "{k}: {x} != {y}"
                );
            }
        }
    }
}
#[test]
fn hierarchy_and_raw_40_bands_match_and_preserve_order() {
    for n in [1, 36, 40] {
        let raw = Synthetic::new(n, false, false);
        let indexed = Synthetic::new(n, true, false);
        let g = polygon(1.125, 96.5);
        let a = single(&raw, &g, &options()).unwrap();
        let b = single(&indexed, &g, &options()).unwrap();
        compare(&a["bands"], &b["bands"]);
        assert_eq!(b["bands"].as_array().unwrap().len(), n);
        assert!(b["work"]["summary_nodes"].as_u64().unwrap() > 0);
        assert!(raw.reads.borrow().iter().all(|r| r.2.len() <= 20));
    }
    let s = Synthetic::new(40, true, false);
    let mut o = options();
    o.bands = vec![39, 0, 27, 8];
    let r = single(&s, &polygon(-1., 101.), &o).unwrap();
    assert_eq!(
        r["bands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["band"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![39, 0, 27, 8]
    );
}
#[test]
fn stored_hierarchy_avoids_protected_raw_interior() {
    let g = polygon(1., 99.);
    let stored = Synthetic::new(1, true, true);
    let result = single(&stored, &g, &options()).unwrap();
    assert!(
        result["work"]["eligible_raw_interior_tiles_avoided"]
            .as_u64()
            .unwrap()
            > 0
    );
    // Force raw through a histogram request; source capability remains present.
    let mut o = options();
    o.histogram_edges = Some(vec![0., 1000., 2000.]);
    o.statistics.as_mut().unwrap().push("histogram".into());
    let raw = Synthetic::new(1, true, true);
    assert!(
        single(&raw, &g, &o)
            .unwrap_err()
            .to_string()
            .contains("protected raw interior")
    );
}

#[test]
fn stored_source_preserves_thin_native_grid_boundary_support() {
    let source = Synthetic::new(1, true, false);
    let (left, right, bottom, top) = (32.25_f64, 32.250000001_f64, 45.125_f64, 45.875_f64);
    let geometry = json!({"type":"Polygon","coordinates":[[[left,bottom],[right,bottom],[right,top],[left,top],[left,bottom]]]});
    let result = single(&source, &geometry, &options()).unwrap();
    let expected = (right - left) * (top - bottom);
    let actual = result["bands"][0]["covered_cell_equivalents"]
        .as_f64()
        .unwrap();
    assert!(
        actual > 0. && (actual - expected).abs() <= expected * 1e-12,
        "thin support {actual} != {expected}"
    );
    assert_eq!(result["bands"][0]["valid_cell_count"], 1);
    assert_eq!(result["work"]["summary_nodes"], 0);
    assert!(
        (result["bands"][0]["fractional_sum"].as_f64().unwrap() - expected * 994.5).abs()
            <= expected * 1e-9
    );
}
#[test]
fn shared_batch_40_groups_and_source_summaries_match_raw() {
    for n in [1, 36, 40] {
        let raw = Synthetic::new(n, false, false);
        let indexed = Synthetic::new(n, true, false);
        let run = |source: &Synthetic| {
            let spec:JobSpec=serde_json::from_value(json!({"zones":[{"id":"a","version":"1","geometry":polygon(1.125,96.5)},{"id":"b","version":"1","geometry":polygon(2.5,95.25)}],"slices":[{"id":"one","source":"r"}],"crs":"LOCAL","tile_edge":32,"options":options()})).unwrap();
            let mut job = Job::new(spec, None, 4096, 1 << 30).unwrap();
            job.next(
                2,
                |_| Ok(Box::new(BorrowedSource(source))),
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let a = run(&raw);
        let b = run(&indexed);
        assert_eq!(b["complete"], true);
        for i in 0..2 {
            compare(&a["rows"][i]["bands"], &b["rows"][i]["bands"]);
        }
        assert!(
            b["metrics"]["source_summary_records_read"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(b["metrics"]["geometry_compilations"], 2);
        assert!(raw.reads.borrow().iter().all(|r| r.2.len() <= 20));
        let unique = raw
            .reads
            .borrow()
            .iter()
            .map(|r| (r.0, r.1))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(raw.reads.borrow().len(), unique * n.div_ceil(20));
    }
}
#[test]
fn summary_layout_and_state_reject_invalid_inputs() {
    let mut s = Synthetic::new(1, true, false);
    s.layout.levels[1].2 += 1;
    assert!(s.layout.validate(&s.meta.grid).is_err());
    assert!(validated_state([1., 0.], 0, 0., 0., 10).is_err());
    assert!(validated_state([1., 0.], 11, 0., 1., 10).is_err());
    assert!(validated_state([f64::NAN, 0.], 1, 0., 1., 10).is_err());
    assert!(validated_state([100., 0.], 1, 0., 1., 10).is_err());
}

#[cfg(feature = "exactextract")]
#[test]
fn optional_bridge_handles_40_distinct_bands_without_wide_reads() {
    use raster_engine::{
        backend::EeOptions,
        exactextract::{self, Input},
    };
    let cancel = AtomicBool::new(false);
    for strategy in ["feature-sequential", "raster-sequential"] {
        for n in [36, 40] {
            let source = Synthetic::new(n, true, true);
            // Upstream uses raw windows even when source summaries exist; allow
            // them here and verify no summary capability was consulted.
            let source = Synthetic {
                reject_interior: false,
                ..source
            };
            let mut o = options();
            o.statistics.as_mut().unwrap().retain(|s| s != "count");
            let inputs = [Input {
                source: &source,
                bands: (0..n).collect(),
            }];
            let zones = [polygon(1.125, 96.5), polygon(2.5, 95.25)];
            let out = exactextract::execute(
                &inputs,
                &zones,
                "LOCAL",
                &o,
                &EeOptions {
                    strategy: strategy.into(),
                    ..EeOptions::default()
                },
                &cancel,
                &mut TileCache::default(),
                1 << 30,
            )
            .unwrap();
            assert_eq!(out.band_count, n);
            assert_eq!(source.summary_reads.get(), 0);
            assert!(source.reads.borrow().iter().all(|r| r.2.len() == 1));
            for zone in 0..2 {
                let rows = out.bands(zone, 0, o.statistics.as_ref().unwrap());
                assert_eq!(rows.len(), n);
                for (b, row) in rows.iter().enumerate() {
                    assert_eq!(row["band"], b);
                    assert!(row["fractional_sum"].as_f64().unwrap() > 0.);
                    if b > 0 {
                        assert!(
                            row["coverage_weighted_mean"].as_f64().unwrap()
                                > rows[b - 1]["coverage_weighted_mean"].as_f64().unwrap()
                        );
                    }
                }
            }
        }
    }
}
