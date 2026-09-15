//! Independent geometry/reducer oracle and cancellation probes for native jobs.
use anyhow::{Result, ensure};
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};
use raster_engine::{
    batch::{BorrowedSource, Job, JobSpec},
    model::{Band, Grid, Raster},
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Source<'a> {
    meta: RasterMetadata,
    values: Vec<Vec<f64>>,
    masks: Vec<Vec<bool>>,
    cancel_after_read: Option<&'a AtomicBool>,
    reads: AtomicUsize,
}
impl<'a> Source<'a> {
    fn new(version: usize, cancel: Option<&'a AtomicBool>) -> Self {
        let width = 64;
        let height = 2;
        let values = (0..3)
            .map(|b| {
                (0..width * height)
                    .map(|i| match b {
                        0 => (i % 3) as f64,
                        1 => 1e16 + (i % 7) as f64 * 2.,
                        _ => {
                            if i % 11 == 0 {
                                -1.
                            } else {
                                (i % 5) as f64
                            }
                        }
                    })
                    .collect()
            })
            .collect();
        let masks = (0..3)
            .map(|b| {
                (0..width * height)
                    .map(|i| (i + version * 3 + b) % 13 != 0)
                    .collect()
            })
            .collect();
        Self {
            meta: RasterMetadata {
                grid: Grid {
                    width,
                    height,
                    transform: [0., 1., 0., 2., 0., -1.],
                    crs: "LOCAL".into(),
                },
                source_id: format!("independent-batch-source-{version}"),
                bands: (0..3)
                    .map(|_| BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: None,
                        block_size: (32, 2),
                    })
                    .collect(),
            },
            values,
            masks,
            cancel_after_read: cancel,
            reads: AtomicUsize::new(0),
        }
    }
}
impl WindowSource for Source<'_> {
    fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        indices: &[usize],
        max: usize,
        _: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        ensure!(
            self.read_buffer_bound(w, h, indices)? <= max,
            "fixture read budget"
        );
        let mut grid = self.meta.grid.clone();
        grid.width = w;
        grid.height = h;
        grid.transform[0] += x as f64;
        grid.transform[3] -= y as f64;
        let bands = indices
            .iter()
            .map(|&b| {
                let mut values = vec![];
                let mut valid = vec![];
                for row in y..y + h {
                    for col in x..x + w {
                        let i = row * self.meta.grid.width + col;
                        values.push(self.values[b][i]);
                        valid.push(self.masks[b][i]);
                    }
                }
                Band {
                    values,
                    valid,
                    unit: None,
                }
            })
            .collect();
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(cancel) = self.cancel_after_read {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok((
            Raster {
                grid,
                bands,
                source_id: self.meta.source_id.clone(),
            },
            ReadMetrics::default(),
        ))
    }
}
fn rectangle(bounds: [f64; 4]) -> Vec<[f64; 2]> {
    let [x0, y0, x1, y1] = bounds;
    vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]
}
fn spec(schedule: &str, layout: &str, options: Value) -> JobSpec {
    serde_json::from_value(json!({"zones":[
        {"id":"outer","version":"v1","geometry":{"type":"Polygon","coordinates":[rectangle([0.25,0.25,63.75,1.75])]}},
        {"id":"hole","version":"v1","geometry":{"type":"Polygon","coordinates":[rectangle([0.25,0.25,63.75,1.75]),rectangle([20.25,0.5,40.75,1.5])]}},
        {"id":"overlap","version":"v1","geometry":{"type":"Polygon","coordinates":[rectangle([7.25,0.,30.5,2.])]}},
    ],"slices":[{"id":"a","source":"a"},{"id":"b","source":"b"}],"crs":"LOCAL","schedule":schedule,"geometry_layout":layout,"tile_edge":32,"options":options})).unwrap()
}
fn coverage(zone: &str, row: usize, col: usize) -> f64 {
    let cell = [
        col as f64,
        1. - row as f64,
        col as f64 + 1.,
        2. - row as f64,
    ];
    let overlap = |r: [f64; 4]| {
        (cell[2].min(r[2]) - cell[0].max(r[0])).max(0.)
            * (cell[3].min(r[3]) - cell[1].max(r[1])).max(0.)
    };
    match zone {
        "outer" => overlap([0.25, 0.25, 63.75, 1.75]),
        "hole" => overlap([0.25, 0.25, 63.75, 1.75]) - overlap([20.25, 0.5, 40.75, 1.5]),
        _ => overlap([7.25, 0., 30.5, 2.]),
    }
}
fn reference(
    source: &Source,
    zone: &str,
    band: usize,
    weighted: bool,
) -> (Option<f64>, [BigRational; 3], Option<f64>, usize) {
    let mut pairs = vec![];
    let mut categories: [BigRational; 3] = std::array::from_fn(|_| BigRational::zero());
    let mut count = 0;
    for i in 0..128 {
        let f = coverage(zone, i / 64, i % 64);
        if f <= 0. || !source.masks[band][i] {
            continue;
        }
        let v = source.values[band][i];
        let mut mass = BigRational::from_float(f).unwrap();
        if weighted {
            if !source.masks[2][i] || source.values[2][i] < 0. {
                continue;
            };
            mass *= BigRational::from_float(source.values[2][i]).unwrap();
        }
        if !mass.is_zero() {
            count += 1;
        }
        if band == 0 {
            categories[v as usize] += &mass;
        }
        pairs.push((v, BigRational::from_float(v).unwrap(), mass));
    }
    let total: BigRational = pairs.iter().map(|(_, _, w)| w).sum();
    if total.is_zero() {
        return (None, categories, None, count);
    }
    let mean = pairs.iter().map(|(_, x, w)| x * w).sum::<BigRational>() / &total;
    let variance = (pairs
        .iter()
        .map(|(_, x, w)| {
            let d = x - &mean;
            &d * &d * w
        })
        .sum::<BigRational>()
        / &total)
        .to_f64();
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cumulative = BigRational::zero();
    let mut median = None;
    for (value, _, mass) in pairs {
        if mass.is_zero() {
            continue;
        }
        cumulative += mass;
        if &cumulative * BigRational::from_integer(2.into()) >= total {
            median = Some(value);
            break;
        }
    }
    (variance, categories, median, count)
}
fn close(actual: &Value, expected: Option<f64>) {
    match expected {
        None => assert!(actual.is_null()),
        Some(expected) => {
            let actual = actual.as_f64().unwrap();
            let ulp = expected - expected.next_down();
            assert!(
                (actual - expected).abs() <= expected.abs() * 1e-10 + 8. * ulp,
                "{actual} != {expected}"
            );
        }
    }
}

#[test]
fn batch_layouts_and_schedules_match_independent_masked_distribution_oracle() {
    let a = Source::new(0, None);
    let b = Source::new(1, None);
    let cancel = AtomicBool::new(false);
    for schedule in ["feature", "tile", "mixed"] {
        for layout in ["csr", "compact", "compact_shared"] {
            let s = spec(
                schedule,
                layout,
                json!({"bands":[0],"weight_band":2,"statistics":["variance","weighted_variance","categories","majority","variety","median","weighted_median","quantiles"],"category_values":[0.,1.,2.],"quantiles":[0.,0.5,1.]}),
            );
            let mut job = Job::new(s, None, 32_768, 1024 << 20).unwrap();
            let mut rows = vec![];
            loop {
                let page = job
                    .next(
                        2,
                        |slice| {
                            Ok(Box::new(BorrowedSource(if slice.id == "a" {
                                &a
                            } else {
                                &b
                            })))
                        },
                        &cancel,
                    )
                    .unwrap();
                rows.extend(page["rows"].as_array().unwrap().clone());
                if page["complete"] == true {
                    break;
                }
            }
            assert_eq!(rows.len(), 6);
            assert_eq!(job.metrics.geometry_compilations, 3);
            assert_eq!(job.metrics.geometry_cache_hits, 3);
            for row in rows {
                let source = if row["slice_id"] == "a" { &a } else { &b };
                let zone = row["zone_id"].as_str().unwrap();
                let r = &row["bands"][0];
                let (variance, masses, median, count) = reference(source, zone, 0, false);
                let (weighted, _, weighted_median, _) = reference(source, zone, 0, true);
                close(&r["variance"], variance);
                close(&r["weighted_variance"], weighted);
                assert_eq!(r["valid_cell_count"], count);
                assert_eq!(r["median"], json!(median));
                assert_eq!(r["weighted_median"], json!(weighted_median));
                assert_eq!(
                    r["categories"]["covered_cell_equivalents"],
                    json!(
                        masses
                            .iter()
                            .map(|v| v.to_f64().unwrap())
                            .collect::<Vec<_>>()
                    )
                );
                let mut winner = 0;
                for i in 1..3 {
                    if masses[i] > masses[winner] {
                        winner = i
                    }
                }
                assert_eq!(r["majority"].as_f64(), Some(winner as f64));
                assert_eq!(r["variety"], 3);
            }
        }
    }
}

#[test]
fn batch_variance_with_large_offsets_matches_exact_centered_reference() {
    let a = Source::new(0, None);
    let b = Source::new(1, None);
    let cancel = AtomicBool::new(false);
    let mut job=Job::new(spec("tile","compact_shared",json!({"bands":[1],"weight_band":2,"statistics":["variance","stddev","weighted_variance"]})),None,32_768,1024<<20).unwrap();
    loop {
        let page = job
            .next(
                4,
                |slice| {
                    Ok(Box::new(BorrowedSource(if slice.id == "a" {
                        &a
                    } else {
                        &b
                    })))
                },
                &cancel,
            )
            .unwrap();
        for row in page["rows"].as_array().unwrap() {
            let source = if row["slice_id"] == "a" { &a } else { &b };
            let zone = row["zone_id"].as_str().unwrap();
            close(
                &row["bands"][0]["variance"],
                reference(source, zone, 1, false).0,
            );
            close(
                &row["bands"][0]["weighted_variance"],
                reference(source, zone, 1, true).0,
            );
        }
        if page["complete"] == true {
            break;
        }
    }
}

#[test]
fn cancellation_after_window_read_prevents_any_reduction_or_publication() {
    for stats in [json!(["sum"]), json!(["variance", "median"])] {
        let cancel = AtomicBool::new(false);
        let source = Source::new(0, Some(&cancel));
        let mut job = Job::new(
            spec(
                "tile",
                "compact_shared",
                json!({"bands":[0],"statistics":stats}),
            ),
            None,
            32_768,
            1024 << 20,
        )
        .unwrap();
        let error = job
            .next(2, |_| Ok(Box::new(BorrowedSource(&source))), &cancel)
            .unwrap_err();
        assert!(error.to_string().contains("cancel"));
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            job.metrics.raw_band_cell_visits, 0,
            "cancelled source must not be reduced"
        );
        assert_eq!(job.metrics.shared_range_reductions, 0);
        assert_eq!(job.metrics.output_rows, 0);
        assert_eq!(job.checkpoint().next_row, 0);
    }
}

#[test]
fn sample_budget_failure_publishes_no_partial_slice() {
    let source = Source::new(0, None);
    let cancel = AtomicBool::new(false);
    let mut job = Job::new(
        spec(
            "tile",
            "compact",
            json!({"bands":[0],"statistics":["median"],"quantile_max_samples":10}),
        ),
        None,
        32_768,
        1024 << 20,
    )
    .unwrap();
    assert!(
        job.next(2, |_| Ok(Box::new(BorrowedSource(&source))), &cancel)
            .unwrap_err()
            .to_string()
            .contains("sample budget")
    );
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        1,
        "first-tile reducer failure must prevent later source reads"
    );
    assert_eq!(job.metrics.output_rows, 0);
    assert_eq!(job.checkpoint().next_row, 0);
}
