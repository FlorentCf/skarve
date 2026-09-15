use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    batch::BorrowedSource,
    model::{Band, Grid, Raster, Sum},
    persistent::TileSummary,
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
    stored_summary::{StoredSummarySource, SummaryLayout, validated_state},
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Synthetic {
    meta: RasterMetadata,
    layout: SummaryLayout,
    enabled: bool,
    cap: usize,
    bound_floor: usize,
    cancel_prefetch: bool,
    bad_value: bool,
    reads: RefCell<Vec<(usize, usize, Vec<usize>)>>,
    events: RefCell<Vec<String>>,
    preparations: RefCell<Vec<([[usize; 4]; 2], Vec<usize>, usize)>>,
}
impl Synthetic {
    fn new(bands: usize, enabled: bool) -> Self {
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
            enabled,
            cap: 64,
            bound_floor: 0,
            cancel_prefetch: false,
            bad_value: false,
            reads: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
            preparations: RefCell::new(Vec::new()),
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
        Some(self)
    }
    fn max_read_bands(&self) -> usize {
        self.cap
    }
    fn read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        Ok((w * h * bands.len() * 9 + 131072).max(self.bound_floor))
    }
    fn boundary_prefetch_enabled(&self) -> bool {
        self.enabled
    }
    fn prefetch_boundary_windows(
        &self,
        windows: &[[usize; 4]],
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<()> {
        raster_engine::model::check_cancel(cancel)?;
        ensure!(
            windows.len() == 2 && bands.len() <= self.cap,
            "invalid prefetch selection"
        );
        self.preparations
            .borrow_mut()
            .push(([windows[0], windows[1]], bands.to_vec(), max_bytes));
        if self.cancel_prefetch {
            cancel.store(true, Ordering::Relaxed);
        }
        raster_engine::model::check_cancel(cancel)
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
            bands.len() <= self.cap && self.read_buffer_bound(w, h, bands)? <= max,
            "synthetic read exceeded group budget"
        );
        ensure!(
            !(x <= 32 && y <= 32 && x + w >= 64 && y + h >= 64),
            "protected raw interior"
        );
        self.reads.borrow_mut().push((x, y, bands.to_vec()));
        self.events
            .borrow_mut()
            .push(format!("raw:{x}:{y}:{w}:{h}:{bands:?}"));
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
                    values: samples
                        .iter()
                        .map(|s| if self.bad_value { f64::INFINITY } else { s.0 })
                        .collect(),
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
        self.events
            .borrow_mut()
            .push(format!("summary:{record}:{bands:?}"));
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
fn query(
    s: &dyn WindowSource,
    g: &Value,
    bands: Vec<usize>,
    cache: &mut TileCache,
    cancel: &AtomicBool,
) -> Result<Value> {
    let mut o = options();
    o.bands = bands;
    streaming::measure_cached_source(s, g, "LOCAL", &o, cancel, cache, 0.)
}
fn run(s: &Synthetic, g: &Value, bands: Vec<usize>) -> Result<Value> {
    query(
        s,
        g,
        bands,
        &mut TileCache::default(),
        &AtomicBool::new(false),
    )
}
#[test]
fn paired_boundaries_preserve_exact_order_masks_and_permuted_bands() {
    for bands in [vec![0], vec![39, 0, 27, 8], (0..40).rev().collect()] {
        let eager = Synthetic::new(40, false);
        let paired = Synthetic::new(40, true);
        let g = polygon(1.125, 98.5);
        let a = run(&eager, &g, bands.clone()).unwrap();
        // The forwarding wrapper is the same one used by shared source jobs.
        let b = query(
            &BorrowedSource(&paired),
            &g,
            bands.clone(),
            &mut TileCache::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(a["bands"], b["bands"]);
        assert_eq!(*eager.events.borrow(), *paired.events.borrow());
        assert!(
            paired
                .events
                .borrow()
                .iter()
                .any(|e| e.starts_with("summary:"))
        );
        assert!(paired.preparations.borrow().len() > 0);
        assert_eq!(b["work"]["boundary_queue_peak_leaves"], 2);
        let charge = b["work"]["boundary_queue_peak_capacity_bytes"]
            .as_u64()
            .unwrap();
        assert!(charge > 0 && charge < 64 << 20);
        for (windows, selected, budget) in paired.preparations.borrow().iter() {
            assert_eq!(selected, &bands);
            assert!(*budget < 64 << 20);
            for w in windows {
                assert!(paired.read_buffer_bound(w[2], w[3], selected).unwrap() <= *budget);
                assert!(!(w[0] <= 32 && w[1] <= 32 && w[0] + w[2] >= 64 && w[1] + w[3] >= 64));
            }
        }
        for key in [
            "summary_nodes",
            "summary_records_read",
            "raw_tiles",
            "raw_positive_cells",
        ] {
            assert_eq!(a["work"][key], b["work"][key]);
        }
        assert_eq!(
            a["streaming"]["decoded_value_bytes"],
            b["streaming"]["decoded_value_bytes"]
        );
    }
}
#[test]
fn one_leaf_and_thin_positive_support_remain_eager() {
    let source = Synthetic::new(1, true);
    // Keep this required boundary leaf separate from the center leaf that
    // the shared fixture denies to detect raw reads of summarized interiors.
    let g = json!({"type":"Polygon","coordinates":[[[12.25,45.125],[12.250000001,45.125],[12.250000001,45.875],[12.25,45.875],[12.25,45.125]]]});
    let answer = run(&source, &g, vec![0]).unwrap();
    assert!(
        answer["bands"][0]["covered_cell_equivalents"]
            .as_f64()
            .unwrap()
            > 0.
    );
    assert_eq!(answer["work"]["boundary_read_ahead"], false);
    assert_eq!(answer["work"]["boundary_queue_peak_leaves"], 0);
    assert!(source.preparations.borrow().is_empty());
}
#[test]
fn full_band_cap_and_queue_budget_fall_back_without_changing_groups() {
    for (bands, cap, floor) in [(40, 20, 0), (1, 64, 64 << 20)] {
        let eager = Synthetic {
            cap,
            bound_floor: floor,
            ..Synthetic::new(bands, false)
        };
        let paired = Synthetic {
            cap,
            bound_floor: floor,
            ..Synthetic::new(bands, true)
        };
        let g = polygon(1.125, 98.5);
        let a = run(&eager, &g, vec![]).unwrap();
        let b = run(&paired, &g, vec![]).unwrap();
        assert_eq!(a["bands"], b["bands"]);
        assert_eq!(*eager.events.borrow(), *paired.events.borrow());
        assert!(paired.preparations.borrow().is_empty());
        assert_eq!(b["work"]["boundary_queue_peak_leaves"], 0);
        assert!(paired.reads.borrow().iter().all(|r| r.2.len() <= cap));
    }
}
#[test]
fn decoded_cache_hits_never_prepare_and_do_not_change_answers() {
    let source = Synthetic::new(4, true);
    let g = polygon(1.125, 98.5);
    let mut cache = TileCache::default();
    cache.set_limit(16 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let first = query(&source, &g, vec![], &mut cache, &cancel).unwrap();
    assert!(!source.preparations.borrow().is_empty());
    source.preparations.borrow_mut().clear();
    source.reads.borrow_mut().clear();
    let second = query(&source, &g, vec![], &mut cache, &cancel).unwrap();
    assert_eq!(first["bands"], second["bands"]);
    assert!(source.preparations.borrow().is_empty());
    assert!(source.reads.borrow().is_empty());
    assert_eq!(second["streaming"]["tiles_read"], 0);
}
#[test]
fn cancellation_during_preparation_prevents_any_following_raw_read() {
    let source = Synthetic {
        cancel_prefetch: true,
        ..Synthetic::new(1, true)
    };
    let g = json!({"type":"Polygon","coordinates":[[[0.25,99.1],[98.5,99.1],[98.5,99.8],[0.25,99.8],[0.25,99.1]]]});
    let cancel = AtomicBool::new(false);
    assert!(query(&source, &g, vec![], &mut TileCache::default(), &cancel).is_err());
    assert!(cancel.load(Ordering::Relaxed));
    assert_eq!(source.preparations.borrow().len(), 1);
    assert!(source.reads.borrow().is_empty());
}
#[test]
fn prepared_arbitrary_reader_still_gets_full_finite_value_validation() {
    let source = Synthetic {
        bad_value: true,
        ..Synthetic::new(1, true)
    };
    let error = run(&source, &polygon(1.125, 98.5), vec![]).unwrap_err();
    assert!(!source.preparations.borrow().is_empty());
    assert!(error.to_string().contains("finite"), "{error:#}");
}
#[test]
fn contains_does_not_change_cache_statistics_or_eviction_order() {
    let source = Synthetic::new(1, false);
    let (raster, _) = source
        .read_selected_window_cancellable(0, 0, 1, 1, &[0], 64 << 20, &AtomicBool::new(false))
        .unwrap();
    let raster = Arc::new(raster);
    let mut cache = TileCache::default();
    cache.set_limit(1 << 20).unwrap();
    cache.insert("a".into(), raster.clone());
    let one = cache.bytes();
    cache.set_limit(2 * one).unwrap();
    cache.insert("b".into(), raster.clone());
    let before = serde_json::to_value(cache.stats()).unwrap();
    assert!(cache.contains("a"));
    assert!(!cache.contains("missing"));
    assert_eq!(before, serde_json::to_value(cache.stats()).unwrap());
    cache.insert("c".into(), raster);
    assert!(!cache.contains("a"));
    assert!(cache.contains("b"));
    assert!(cache.contains("c"));
}
#[test]
fn http_policy_defaults_on_and_allows_explicit_eager_ablation() {
    assert!(raster_engine::source::HttpOptions::default().boundary_read_ahead);
    let off: raster_engine::source::HttpOptions =
        serde_json::from_value(json!({"boundary_read_ahead":false})).unwrap();
    assert!(!off.boundary_read_ahead);
}

#[test]
fn one_cached_leaf_is_never_in_a_prepared_pair() {
    let source = Synthetic::new(1, true);
    let window = [0, 0, 32, 32];
    let bands = [0];
    let cancel = AtomicBool::new(false);
    let (raster, _) = source
        .read_selected_window_cancellable(0, 0, 32, 32, &bands, 64 << 20, &cancel)
        .unwrap();
    source.reads.borrow_mut().clear();
    source.events.borrow_mut().clear();
    let identity = raster_engine::tile_cache::source_key(&source).unwrap();
    let key = raster_engine::tile_cache::window_key(&identity, window, &bands);
    let mut cache = TileCache::default();
    cache.set_limit(16 << 20).unwrap();
    cache.insert(key, Arc::new(raster));
    let cached = query(&source, &polygon(1.125, 98.5), vec![0], &mut cache, &cancel).unwrap();
    let eager = Synthetic::new(1, false);
    let expected = run(&eager, &polygon(1.125, 98.5), vec![0]).unwrap();
    assert_eq!(cached["bands"], expected["bands"]);
    assert_eq!(cache.stats().hits, 1);
    assert!(
        source
            .preparations
            .borrow()
            .iter()
            .all(|(windows, _, _)| !windows.contains(&window))
    );
}
