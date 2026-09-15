use anyhow::{Result, bail, ensure};
use raster_engine::{
    model::{Grid, Raster, check_cancel},
    ordered_source::{self, OrderedBudget, OrderedPolygon, OrderedRequest, OrderedWindow},
    source::{
        BandMetadata, RasterMetadata, RawBandMetadata, RawBandWindow, RawRasterMetadata,
        RawScalarType, RawWindow, ReadMetrics, WindowSource,
    },
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct Read {
    rectangle: [usize; 4],
    bands: Vec<usize>,
}
struct Source {
    metadata: RasterMetadata,
    raw: RawRasterMetadata,
    values: Vec<Vec<f64>>,
    reads: RefCell<Vec<Read>>,
    checks: Cell<usize>,
    max_cells: usize,
    max_bands: usize,
    physical_block_cost: bool,
    pixel_block_cost: bool,
    invalidate: bool,
    cancel_on_read: bool,
}
impl Source {
    fn new(values: Vec<Vec<f64>>, width: usize, kind: RawScalarType) -> Self {
        assert!(values.iter().all(|v| v.len() == values[0].len()));
        let height = values[0].len() / width;
        let bands = values.len();
        Self {
            metadata: RasterMetadata {
                grid: Grid {
                    width,
                    height,
                    transform: [0., 1., 0., height as f64, 0., -1.],
                    crs: "LOCAL".into(),
                },
                source_id: "ordered-fixture".into(),
                bands: (0..bands)
                    .map(|_| BandMetadata {
                        data_type: format!("{kind:?}"),
                        nodata: Some(-99.),
                        scale: 2.,
                        offset: 100.,
                        unit: None,
                        block_size: (256, 256),
                    })
                    .collect(),
            },
            raw: RawRasterMetadata {
                bands: (0..bands)
                    .map(|b| RawBandMetadata {
                        scalar_type: kind,
                        nodata_f64_bits: Some((-99f64).to_bits()),
                        scale_f64_bits: 2f64.to_bits(),
                        offset_f64_bits: 100f64.to_bits(),
                        unit: None,
                        mask_flags: 0,
                        original_band_index: b,
                        description: String::new(),
                    })
                    .collect(),
                pixel_convention: "Area".into(),
                source_band_count: bands,
                source_overview: None,
            },
            values,
            reads: RefCell::new(Vec::new()),
            checks: Cell::new(0),
            max_cells: usize::MAX,
            max_bands: 20,
            physical_block_cost: false,
            pixel_block_cost: false,
            invalidate: false,
            cancel_on_read: false,
        }
    }
}
impl WindowSource for Source {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        Some(&self.raw)
    }
    fn max_read_bands(&self) -> usize {
        self.max_bands
    }
    fn verify_immutable(&self) -> Result<()> {
        self.checks.set(self.checks.get() + 1);
        ensure!(
            !self.invalidate || self.checks.get() == 1,
            "source generation changed"
        );
        Ok(())
    }
    fn stored_summaries(&self) -> Option<&dyn raster_engine::stored_summary::StoredSummarySource> {
        panic!("ordered execution must not consult summaries")
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        ensure!(bands.len() <= self.max_bands, "mock band limit");
        Ok(if w * h > self.max_cells {
            1 << 30
        } else {
            w * h
                * bands
                    .iter()
                    .map(|&b| self.raw.bands[b].scalar_type.byte_width() + 1)
                    .sum::<usize>()
                + 1024
                + if self.physical_block_cost {
                    // Model the existing conservative BAND256 decoded-block
                    // bound, including both possible edge blocks and masks.
                    (w.div_ceil(256) + 1) * (h.div_ceil(256) + 1) * 256 * 256 * 10 * bands.len()
                } else {
                    0
                }
                + if self.pixel_block_cost {
                    // Pixel-interleaved decoding includes every physical band
                    // even when only one exposed band is requested.
                    (w.div_ceil(128) + 1)
                        * (h.div_ceil(128) + 1)
                        * 128
                        * 128
                        * (9 * self.raw.bands.len() + bands.len())
                } else {
                    0
                }
        })
    }
    fn read_raw_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max: usize,
        cancel: &AtomicBool,
    ) -> Result<(RawWindow, ReadMetrics)> {
        check_cancel(cancel)?;
        ensure!(
            x + w <= self.metadata.grid.width && y + h <= self.metadata.grid.height,
            "mock bounds"
        );
        ensure!(
            self.raw_read_buffer_bound(w, h, bands)? <= max,
            "mock memory"
        );
        self.reads.borrow_mut().push(Read {
            rectangle: [x, y, w, h],
            bands: bands.to_vec(),
        });
        let raw = RawWindow {
            width: w,
            height: h,
            bands: bands
                .iter()
                .map(|&b| {
                    let mut samples_le = Vec::new();
                    for row in y..y + h {
                        for column in x..x + w {
                            let value = self.values[b][row * self.metadata.grid.width + column];
                            if self.raw.bands[b].scalar_type == RawScalarType::Float32 {
                                samples_le.extend_from_slice(&(value as f32).to_le_bytes());
                            } else {
                                samples_le.extend_from_slice(&value.to_le_bytes());
                            }
                        }
                    }
                    RawBandWindow {
                        samples_le,
                        mask: vec![0; w * h],
                    } // Ignored only by this explicit policy.
                })
                .collect(),
        };
        if self.cancel_on_read {
            cancel.store(true, Ordering::Release);
        }
        Ok((
            raw,
            ReadMetrics {
                raster_io_calls: bands.len(),
                ..Default::default()
            },
        ))
    }
    fn read_selected_window_cancellable(
        &self,
        _x: usize,
        _y: usize,
        _w: usize,
        _h: usize,
        _bands: &[usize],
        _max: usize,
        _cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        bail!("ordered executor must use original typed data")
    }
}
fn indexes(window: [usize; 4], values: Vec<u32>) -> OrderedWindow {
    OrderedWindow {
        window,
        indexes: Some(values),
        runs: None,
    }
}
fn runs(window: [usize; 4], values: Vec<[u32; 2]>) -> OrderedWindow {
    OrderedWindow {
        window,
        indexes: None,
        runs: Some(values),
    }
}
fn request(polygons: Vec<OrderedPolygon>, bands: Vec<usize>) -> OrderedRequest {
    OrderedRequest {
        polygons,
        bands,
        nodata: None,
        budget: OrderedBudget::default(),
    }
}
fn polygon(id: &str, windows: Vec<OrderedWindow>) -> OrderedPolygon {
    OrderedPolygon {
        id: id.into(),
        windows,
    }
}
fn execute(source: &Source, r: &OrderedRequest) -> serde_json::Value {
    ordered_source::execute(source, r, &AtomicBool::new(false)).unwrap()
}

#[test]
fn physical_chunks_and_band_groups_preserve_the_original_window_left_fold() {
    let mut values = vec![2f64.powi(24), (0.5f32 - 2f32.powi(-25)) as f64];
    values.extend(vec![2f64.powi(-29); 32]);
    let mut expected = 0.;
    for &v in &values {
        expected += v;
    }
    let regrouped = values
        .chunks(2)
        .map(|chunk| chunk.iter().copied().sum::<f64>())
        .sum::<f64>();
    assert_ne!(
        expected, regrouped,
        "adversarial fixture must distinguish physical partial sums"
    );
    let source = Source {
        max_cells: 2,
        max_bands: 2,
        ..Source::new(
            vec![values.clone(), values.clone(), values],
            34,
            RawScalarType::Float32,
        )
    };
    let window = indexes([0, 0, 34, 1], (0..34).collect());
    let r = request(
        vec![
            polygon("a", vec![window.clone()]),
            polygon("b", vec![window]),
        ],
        vec![2, 0, 1],
    );
    let out = execute(&source, &r);
    for row in out["rows"].as_array().unwrap() {
        for (b, id) in [2, 0, 1].into_iter().enumerate() {
            assert_eq!(row["bands"][b]["id"], id);
            assert_eq!(
                row["bands"][b]["sum"].as_f64().unwrap().to_bits(),
                expected.to_bits()
            );
            assert_eq!(row["bands"][b]["valid_count"], 34);
            assert_eq!(row["bands"][b]["excluded_mask"], 0);
        }
    }
    assert_eq!(out["metrics"]["logical_windows"], 2);
    assert_eq!(out["metrics"]["unique_windows"], 1);
    assert_eq!(out["metrics"]["shared_window_references"], 1);
    let reads = source.reads.borrow();
    assert!(
        reads
            .iter()
            .all(|r| r.rectangle[2] * r.rectangle[3] <= 2 && r.bands.len() <= 2)
    );
    // Identical polygons share each band-group/physical read, not a reread loop.
    assert_eq!(reads.len(), 34);
    assert!(reads.iter().all(|r| r.bands == [2, 0] || r.bands == [1]));
    assert_eq!(source.checks.get(), 2);
}

#[test]
fn logical_window_order_survives_shared_read_scheduling() {
    let source = Source::new(vec![vec![1e16, 1., 1.]], 3, RawScalarType::Float64);
    let windows = (0..3)
        .map(|x| runs([x, 0, 1, 1], vec![[0, 1]]))
        .collect::<Vec<_>>();
    let r = request(
        vec![
            polygon("large-first", windows.clone()),
            polygon("large-last", windows.iter().rev().cloned().collect()),
        ],
        vec![0],
    );
    let out = execute(&source, &r);
    assert_eq!(out["rows"][0]["bands"][0]["sum"], 1e16);
    assert_eq!(out["rows"][1]["bands"][0]["sum"], 1e16 + 2.);
    assert_eq!(source.reads.borrow().len(), 1);
    assert_eq!(out["metrics"]["shared_window_references"], 3);
}

#[test]
fn compact_runs_match_indexes_and_preserve_raw_filtering_and_nodata_override() {
    let values = vec![
        f64::NAN,
        f64::INFINITY,
        -99.,
        -2.,
        -0.,
        5.,
        7.,
        8.,
        9.,
        10.,
        11.,
        12.,
    ];
    let source = Source {
        max_cells: 4,
        ..Source::new(vec![values], 6, RawScalarType::Float64)
    };
    let selected = vec![0, 1, 2, 3, 4, 5, 8, 9, 11];
    let r = request(
        vec![
            polygon("indexes", vec![indexes([0, 0, 6, 2], selected)]),
            polygon(
                "runs",
                vec![runs([0, 0, 6, 2], vec![[0, 6], [8, 10], [11, 12]])],
            ),
        ],
        vec![0],
    );
    let out = execute(&source, &r);
    assert_eq!(out["rows"][0]["bands"], out["rows"][1]["bands"]);
    let band = &out["rows"][0]["bands"][0];
    assert_eq!(band["sum"], 36.);
    assert_eq!(band["valid_count"], 5);
    assert_eq!(band["excluded_nonfinite"], 2);
    assert_eq!(band["excluded_nodata"], 1);
    assert_eq!(band["excluded_negative"], 1);
    assert_eq!(band["excluded_mask"], 0);
    let mut override_request = r.clone();
    override_request.nodata = Some(vec![Some(0.)]);
    let out = execute(&source, &override_request);
    let band = &out["rows"][0]["bands"][0];
    assert_eq!(band["sum"], 36.);
    assert_eq!(band["valid_count"], 4);
    assert_eq!(band["excluded_negative"], 2);
    assert_eq!(band["excluded_nodata"], 1);
    let mut no_data = r;
    no_data.nodata = Some(vec![None]);
    assert_eq!(
        execute(&source, &no_data)["rows"][0]["bands"][0]["excluded_nodata"],
        0
    );
}

#[test]
fn malformed_selection_controls_and_source_types_fail_before_reads() {
    let source = Source::new(vec![vec![1.; 12]], 6, RawScalarType::Float32);
    let invalid = vec![
        indexes([0, 0, 6, 2], vec![1, 0]),
        indexes([0, 0, 6, 2], vec![1, 1]),
        indexes([0, 0, 6, 2], vec![12]),
        runs([0, 0, 6, 2], vec![[0, 3], [2, 4]]),
        runs([0, 0, 6, 2], vec![[1, 1]]),
        runs([0, 0, 6, 2], vec![[0, 13]]),
        OrderedWindow {
            window: [0, 0, 6, 2],
            indexes: None,
            runs: None,
        },
        OrderedWindow {
            window: [0, 0, 6, 2],
            indexes: Some(vec![0]),
            runs: Some(vec![[0, 1]]),
        },
        indexes([6, 0, 1, 1], vec![0]),
    ];
    for window in invalid {
        assert!(
            ordered_source::execute(
                &source,
                &request(vec![polygon("bad", vec![window])], vec![0]),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    let mut r = request(
        vec![polygon("p", vec![runs([0, 0, 6, 2], vec![[0, 12]])])],
        vec![0],
    );
    r.budget.max_contributions = 11;
    assert!(ordered_source::execute(&source, &r, &AtomicBool::new(false)).is_err());
    r.budget = OrderedBudget {
        planning_bytes: 65536,
        ..Default::default()
    };
    assert_eq!(ordered_source::reservation_bytes(&r).unwrap(), 64 << 20);
    assert!(ordered_source::execute(&source, &r, &AtomicBool::new(false)).is_err());
    r.budget.working_bytes = 512 << 20;
    assert!(ordered_source::reservation_bytes(&r).is_err());
    assert!(source.reads.borrow().is_empty());
    let mut integer = Source::new(vec![vec![1.; 12]], 6, RawScalarType::Float32);
    integer.raw.bands[0].scalar_type = RawScalarType::Byte;
    r.budget = OrderedBudget::default();
    assert!(ordered_source::execute(&integer, &r, &AtomicBool::new(false)).is_err());
    assert!(integer.reads.borrow().is_empty());
}

#[test]
fn read_budgets_cancellation_and_generation_fail_closed() {
    let r = request(
        vec![polygon("p", vec![runs([0, 0, 12, 1], vec![[0, 12]])])],
        vec![0],
    );
    let base = || Source::new(vec![vec![1.; 12]], 12, RawScalarType::Float64);
    let too_large = Source {
        max_cells: 0,
        ..base()
    };
    assert!(
        ordered_source::execute(&too_large, &r, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("minimum read")
    );
    assert!(too_large.reads.borrow().is_empty());
    let limited = Source {
        max_cells: 2,
        ..base()
    };
    let mut low = r.clone();
    low.budget.max_read_calls = 1;
    assert!(
        ordered_source::execute(&limited, &low, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("read-call budget")
    );
    assert_eq!(limited.reads.borrow().len(), 1);
    let byte_limited = Source {
        max_cells: 2,
        ..base()
    };
    let mut low = r.clone();
    low.budget.read_materialized_bytes = 8; // Even one f64 sample plus mask is 9.
    assert!(
        ordered_source::execute(&byte_limited, &low, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("materialized read-byte budget")
    );
    assert!(byte_limited.reads.borrow().is_empty());
    low.budget.read_materialized_bytes = 18;
    assert!(
        ordered_source::execute(&byte_limited, &low, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("materialized read-byte budget")
    );
    assert_eq!(byte_limited.reads.borrow().len(), 1);
    let cancel_source = Source {
        cancel_on_read: true,
        ..base()
    };
    assert!(ordered_source::execute(&cancel_source, &r, &AtomicBool::new(false)).is_err());
    assert_eq!(cancel_source.reads.borrow().len(), 1);
    let changed = Source {
        invalidate: true,
        ..base()
    };
    assert!(
        ordered_source::execute(&changed, &r, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("generation changed")
    );
    let source = base();
    assert!(ordered_source::execute(&source, &r, &AtomicBool::new(true)).is_err());
    assert!(source.reads.borrow().is_empty());
}

#[test]
fn empty_masks_and_empty_logical_windows_have_zero_sums_without_raw_reads() {
    let source = Source {
        max_cells: 0, // Empty masks must not require even one cell of read scratch.
        ..Source::new(vec![vec![1.; 4]], 4, RawScalarType::Float32)
    };
    let r:OrderedRequest=serde_json::from_value(json!({"polygons":[{"id":"empty","windows":[]},{"id":"masked","windows":[{"window":[0,0,4,1],"runs":[]}]}],"bands":[0]})).unwrap();
    let out = execute(&source, &r);
    for row in out["rows"].as_array().unwrap() {
        assert_eq!(row["bands"][0]["sum"], 0.);
        assert_eq!(row["bands"][0]["valid_count"], 0);
    }
    assert!(source.reads.borrow().is_empty());
    assert_eq!(source.checks.get(), 2);
}

fn oracle(source: &Source, polygon: &OrderedPolygon, band: usize) -> f64 {
    let mut total = 0.;
    for window in &polygon.windows {
        let selected = window.indexes.clone().unwrap_or_else(|| {
            window
                .runs
                .as_ref()
                .unwrap()
                .iter()
                .flat_map(|r| r[0]..r[1])
                .collect()
        });
        let [x, y, w, _] = window.window;
        let mut partial = 0.;
        for index in selected {
            let index = index as usize;
            let value =
                source.values[band][(y + index / w) * source.metadata.grid.width + x + index % w];
            let value = if source.raw.bands[band].scalar_type == RawScalarType::Float32 {
                (value as f32) as f64
            } else {
                value
            };
            if value.is_finite() && value >= 0. && value != -99. {
                partial += value;
            }
        }
        total += partial;
    }
    total
}

#[test]
fn overlapping_shifted_windows_share_reads_and_materialization_without_reordering() {
    let values = (0..100)
        .map(|i| {
            if i == 0 {
                2f64.powi(24)
            } else {
                i as f64 / 16.
            }
        })
        .collect::<Vec<_>>();
    let source = Source::new(vec![values.clone()], 10, RawScalarType::Float32);
    let r = request(
        vec![
            polygon("a", vec![runs([0, 0, 8, 8], vec![[0, 64]])]),
            polygon("shifted", vec![runs([1, 1, 8, 8], vec![[0, 64]])]),
            polygon(
                "holes-and-windows",
                vec![
                    runs([5, 5, 4, 4], vec![[0, 3], [5, 7], [11, 16]]),
                    indexes([0, 0, 3, 2], vec![0, 2, 3, 5]),
                ],
            ),
        ],
        vec![0],
    );
    let out = execute(&source, &r);
    for (i, polygon) in r.polygons.iter().enumerate() {
        assert_eq!(
            out["rows"][i]["bands"][0]["sum"]
                .as_f64()
                .unwrap()
                .to_bits(),
            oracle(&source, polygon, 0).to_bits()
        );
    }
    // A genuinely independent single-polygon loop performs separate source reads.
    let mut independent_reads = 0;
    let mut independent_bytes = 0;
    for polygon in &r.polygons {
        let independent = Source::new(vec![values.clone()], 10, RawScalarType::Float32);
        let individual = execute(&independent, &request(vec![polygon.clone()], vec![0]));
        independent_reads += independent.reads.borrow().len();
        independent_bytes += individual["metrics"]["read_materialized_bytes"]
            .as_u64()
            .unwrap();
    }
    assert_eq!(source.reads.borrow().len(), 1);
    assert!(source.reads.borrow().len() < independent_reads);
    assert!(out["metrics"]["read_materialized_bytes"].as_u64().unwrap() < independent_bytes);
    assert_eq!(out["metrics"]["unique_windows"], 4);
    assert_eq!(out["metrics"]["shared_window_references"], 0);
}

#[test]
fn sparse_disjoint_holes_and_multiple_windows_keep_order_with_bounded_overread() {
    let values = (0..30_000)
        .map(|i| {
            if i % 317 == 0 {
                2f64.powi(24)
            } else {
                2f64.powi(-30)
            }
        })
        .collect::<Vec<_>>();
    let source = Source {
        max_cells: 7,
        ..Source::new(vec![values], 50, RawScalarType::Float32)
    };
    let r = request(
        vec![
            polygon(
                "sparse",
                vec![runs([0, 0, 50, 600], vec![[0, 1], [29999, 30000]])],
            ),
            polygon(
                "middle-holes",
                vec![runs([20, 250, 20, 2], vec![[2, 4], [28, 31]])],
            ),
            polygon(
                "opposite-window-order",
                vec![
                    runs([0, 500, 4, 1], vec![[0, 4]]),
                    runs([45, 0, 4, 1], vec![[0, 4]]),
                ],
            ),
        ],
        vec![0],
    );
    let out = execute(&source, &r);
    for (i, polygon) in r.polygons.iter().enumerate() {
        assert_eq!(
            out["rows"][i]["bands"][0]["sum"]
                .as_f64()
                .unwrap()
                .to_bits(),
            oracle(&source, polygon, 0).to_bits()
        );
    }
    let reads = source.reads.borrow();
    assert!(reads.iter().all(|r| r.rectangle[2] * r.rectangle[3] <= 7));
    // Never fetch the large intervening bands of rows with no selected samples.
    assert!(
        reads
            .iter()
            .all(|r| [0, 250, 251, 500, 599].contains(&r.rectangle[1]))
    );
    let bytes: usize = reads
        .iter()
        .map(|r| r.rectangle[2] * r.rectangle[3] * 5)
        .sum();
    assert_eq!(out["metrics"]["read_materialized_bytes"], bytes);
    assert!(bytes < 500);
}

#[test]
fn forty_exposed_bands_use_source_capability_and_explicit_nodata_width() {
    for max_bands in [20, 64] {
        let source = Source {
            max_bands,
            ..Source::new(
                (0..40).map(|b| vec![b as f64, 1., 2., 3.]).collect(),
                4,
                RawScalarType::Float32,
            )
        };
        let mut r = request(
            vec![
                polygon("full", vec![runs([0, 0, 4, 1], vec![[0, 4]])]),
                polygon("shifted", vec![runs([1, 0, 3, 1], vec![[0, 3]])]),
            ],
            vec![],
        );
        r.nodata = Some(vec![None; 40]);
        assert_eq!(ordered_source::reservation_bytes(&r).unwrap(), 64 << 20);
        let out = execute(&source, &r);
        assert_eq!(out["rows"][0]["bands"].as_array().unwrap().len(), 40);
        for b in 0..40 {
            assert_eq!(out["rows"][0]["bands"][b]["id"], b);
            assert_eq!(out["rows"][0]["bands"][b]["sum"], b as f64 + 6.);
            assert_eq!(out["rows"][1]["bands"][b]["sum"], 6.);
        }
        assert_eq!(out["metrics"]["read_materialized_bytes"], 4 * 40 * 5);
        assert_eq!(source.reads.borrow().len(), 40usize.div_ceil(max_bands));
        assert!(
            source
                .reads
                .borrow()
                .iter()
                .all(|r| r.bands.len() <= max_bands)
        );
    }
}

#[test]
fn nonfinite_accumulator_overflow_does_not_return_a_partial_result() {
    let source = Source::new(vec![vec![f64::MAX; 2]], 2, RawScalarType::Float64);
    for windows in [
        vec![runs([0, 0, 2, 1], vec![[0, 2]])],
        vec![
            runs([0, 0, 1, 1], vec![[0, 1]]),
            runs([1, 0, 1, 1], vec![[0, 1]]),
        ],
    ] {
        let err = ordered_source::execute(
            &source,
            &request(vec![polygon("overflow", windows)], vec![0]),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("sum overflow"));
    }
}

#[test]
fn shared_stripes_stop_at_last_selected_row_in_a_tall_source_and_window() {
    let source = Source::new(
        vec![(0..10_000).map(|i| i as f64 / 16.).collect()],
        10,
        RawScalarType::Float64,
    );
    let r = request(
        vec![
            polygon("short-runs", vec![runs([1, 100, 5, 300], vec![[0, 10]])]),
            polygon(
                "shifted-short-indexes",
                vec![indexes([2, 101, 5, 200], vec![0, 1, 5, 6])],
            ),
        ],
        vec![0],
    );
    let out = execute(&source, &r);
    for (i, polygon) in r.polygons.iter().enumerate() {
        assert_eq!(
            out["rows"][i]["bands"][0]["sum"]
                .as_f64()
                .unwrap()
                .to_bits(),
            oracle(&source, polygon, 0).to_bits()
        );
    }
    let reads = source.reads.borrow();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].rectangle, [1, 100, 6, 3]);
    assert!(
        reads
            .iter()
            .all(|read| read.rectangle[1] + read.rectangle[3] <= 103)
    );
    assert_eq!(out["metrics"]["read_materialized_bytes"], 6 * 3 * 9);
}

#[test]
fn band_admission_preserves_spatial_reads_for_shifted_many_band_windows() {
    let mut source = Source::new(
        (0..36)
            .map(|band| {
                (0..270 * 170)
                    .map(|cell| ((cell * 17 + band * 29) % 32771) as f64 / 16.)
                    .collect()
            })
            .collect(),
        270,
        RawScalarType::Float32,
    );
    source.physical_block_cost = true;
    let r = request(
        vec![
            polygon("first", vec![runs([3, 2, 255, 162], vec![[0, 255 * 162]])]),
            polygon(
                "shifted",
                vec![runs([5, 3, 255, 162], vec![[0, 255 * 162]])],
            ),
        ],
        (0..36).rev().collect(),
    );
    let out = execute(&source, &r);
    for (p, polygon) in r.polygons.iter().enumerate() {
        for (b, &id) in r.bands.iter().enumerate() {
            assert_eq!(out["rows"][p]["bands"][b]["id"], id);
            assert_eq!(
                out["rows"][p]["bands"][b]["sum"]
                    .as_f64()
                    .unwrap()
                    .to_bits(),
                oracle(&source, polygon, id).to_bits()
            );
        }
    }
    let reads = source.reads.borrow();
    assert!(
        reads.len() <= 6,
        "avoid repeated physical block decoding: {} reads",
        reads.len()
    );
    assert!(reads.iter().all(|read| read.rectangle[3] >= 160));
    assert!(reads.iter().all(|read| read.bands.len() < 20));
    assert!(out["metrics"]["read_bound_peak_bytes"].as_u64().unwrap() < 64 << 20);
}

#[test]
fn spatial_admission_does_not_repeat_all_physical_bands_for_each_selected_band() {
    let mut source = Source::new(vec![vec![1.25; 270 * 170]; 36], 270, RawScalarType::Float32);
    source.pixel_block_cost = true;
    let r = request(
        vec![
            polygon("first", vec![runs([3, 2, 255, 162], vec![[0, 255 * 162]])]),
            polygon(
                "shifted",
                vec![runs([1, 2, 256, 162], vec![[0, 256 * 162]])],
            ),
        ],
        (0..36).rev().collect(),
    );
    let out = execute(&source, &r);
    for (p, polygon) in r.polygons.iter().enumerate() {
        for (b, &id) in r.bands.iter().enumerate() {
            assert_eq!(out["rows"][p]["bands"][b]["id"], id);
            assert_eq!(
                out["rows"][p]["bands"][b]["sum"]
                    .as_f64()
                    .unwrap()
                    .to_bits(),
                oracle(&source, polygon, id).to_bits()
            );
        }
    }
    let reads = source.reads.borrow();
    assert!(
        reads.len() <= 8,
        "avoid all-band decode for single-band passes: {} reads",
        reads.len()
    );
    assert!(reads.iter().all(|read| read.bands.len() > 1));
    assert!(out["metrics"]["read_bound_peak_bytes"].as_u64().unwrap() < 64 << 20);
}
