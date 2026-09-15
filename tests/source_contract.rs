//! An independently instrumented procedural source, with no TIFF or value file.
use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    model::{Band, Grid, Raster, check_cancel},
    persistent::{self, BoundarySource, QueryOptions},
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
    streaming,
    tile_cache::TileCache,
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    sync::atomic::AtomicBool,
};

struct Procedural {
    meta: RasterMetadata,
    protect: Cell<bool>,
    adapter_extra: Cell<usize>,
    blocks: RefCell<Vec<(usize, usize)>>,
}
impl WindowSource for Procedural {
    fn read_buffer_bound(&self, width: usize, height: usize, indices: &[usize]) -> Result<usize> {
        Ok(width * height * indices.len() * 9 + self.adapter_extra.get())
    }
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
        bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        check_cancel(cancel)?;
        ensure!(w * h * indices.len() * 9 <= bytes, "reader budget");
        // This adapter's actual fetch unit is a 16x16 native block. Reject in
        // the reader itself, independently of the engine's optional diagnostic.
        for by in y / 16..(y + h).div_ceil(16) {
            for bx in x / 16..(x + w).div_ceil(16) {
                ensure!(
                    !self.protect.get() || !(2..10).contains(&bx) || !(2..10).contains(&by),
                    "forbidden procedural interior block"
                );
                self.blocks.borrow_mut().push((bx, by));
            }
        }
        let bands = indices
            .iter()
            .map(|&b| {
                let values = (y..y + h)
                    .flat_map(|yy| {
                        (x..x + w).map(move |xx| ((xx * 3 + yy * 5 + b * 7) % 71) as f64 - 20.)
                    })
                    .collect();
                let valid = (y..y + h)
                    .flat_map(|yy| (x..x + w).map(move |xx| (xx + yy + b) % 19 != 0))
                    .collect();
                Band {
                    values,
                    valid,
                    unit: None,
                }
            })
            .collect();
        Ok((
            Raster {
                grid: Grid {
                    width: w,
                    height: h,
                    transform: [x as f64, 1., 0., 193. - y as f64, 0., -1.],
                    crs: "LOCAL".into(),
                },
                bands,
                source_id: self.meta.source_id.clone(),
            },
            ReadMetrics::default(),
        ))
    }
}

#[test]
fn direct_and_persistent_engines_accept_non_file_source_and_skip_actual_interior_fetches() {
    let source = procedural();
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("summaries");
    let cancel = AtomicBool::new(false);
    let built = persistent::build_from_source(
        &source,
        destination.to_str().unwrap(),
        16,
        "band_major",
        "hierarchy",
        BoundarySource::Original,
        &cancel,
    )
    .unwrap();
    assert_eq!(built["duplicate_raw_bytes"], 0);
    assert!(!destination.join("pixels.rsr").exists());
    // Polygon is supplied only after preparation. Its interior in row space is
    // (16.25,175.75)^2. All 64 blocks [2,10)^2 have positive clearance.
    let polygon = json!({"type":"Polygon","coordinates":[[[16.25,17.25],[175.75,17.25],[175.75,176.75],[16.25,176.75],[16.25,17.25]]]});
    let options = Options {
        bands: vec![2, 0],
        statistics: Some(vec![
            "sum".into(),
            "support".into(),
            "min".into(),
            "max".into(),
        ]),
        ..Default::default()
    };
    let expected = streaming::measure_cached_source(
        &source,
        &polygon,
        "LOCAL",
        &options,
        &cancel,
        &mut TileCache::default(),
        0.,
    )
    .unwrap();
    source.blocks.borrow_mut().clear();
    source.protect.set(true);
    let index = destination.join("summary.rsi");
    let index = index.to_str().unwrap();
    let query = |direct| QueryOptions {
        joint_planner: None,
        index_path: index,
        raw_path: "",
        source_path: None,
        expected_build_id: None,
        read_memory_bytes: 1024 * 1024,
        summary_page_bytes: Some(0),
        coalesce_raw: true,
        order_summaries: false,
        forbidden_raw_tiles: &[],
        force_direct: direct,
        hierarchy: None,
    };
    let actual = persistent::query_with_boundary_source(
        query(false),
        &polygon,
        "LOCAL",
        &options,
        &cancel,
        Some(&source),
    )
    .unwrap();
    for (a, b) in actual["bands"]
        .as_array()
        .unwrap()
        .iter()
        .zip(expected["bands"].as_array().unwrap())
    {
        for key in ["fractional_sum", "covered_cell_equivalents", "min", "max"] {
            assert!(
                (a[key].as_f64().unwrap() - b[key].as_f64().unwrap()).abs() < 1e-8,
                "{key}"
            );
        }
    }
    assert_eq!(actual["work"]["eligible_raw_interior_tiles_avoided"], 64);
    assert_eq!(actual["io"]["raw_reads"], 0);
    assert!(!source.blocks.borrow().is_empty());
    assert!(
        source
            .blocks
            .borrow()
            .iter()
            .all(|(x, y)| !(2..10).contains(x) || !(2..10).contains(y))
    );
    source.adapter_extra.set(2 * 1024 * 1024);
    let before = source.blocks.borrow().len();
    let budget_error = persistent::query_with_boundary_source(
        query(false),
        &polygon,
        "LOCAL",
        &options,
        &cancel,
        Some(&source),
    )
    .unwrap_err();
    assert!(budget_error.to_string().contains("memory budget"));
    assert_eq!(source.blocks.borrow().len(), before);
    source.adapter_extra.set(0);
    let error = persistent::query_with_boundary_source(
        query(true),
        &polygon,
        "LOCAL",
        &options,
        &cancel,
        Some(&source),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("forbidden procedural interior block")
    );
}

fn procedural() -> Procedural {
    Procedural {
        meta: RasterMetadata {
            grid: Grid {
                width: 193,
                height: 193,
                transform: [0., 1., 0., 193., 0., -1.],
                crs: "LOCAL".into(),
            },
            bands: (0..3)
                .map(|_| BandMetadata {
                    data_type: "procedural_f64".into(),
                    nodata: None,
                    scale: 1.,
                    offset: 0.,
                    unit: None,
                    block_size: (16, 16),
                })
                .collect(),
            source_id: "procedural-definition-v1".into(),
        },
        protect: Cell::new(false),
        adapter_extra: Cell::new(0),
        blocks: RefCell::new(vec![]),
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Bound,
    Shift,
    Mask,
    Bands,
    Identity,
    Unit,
    Nonfinite,
}
struct Faulty {
    inner: Procedural,
    fault: Cell<Fault>,
}
impl WindowSource for Faulty {
    fn metadata(&self) -> &RasterMetadata {
        self.inner.metadata()
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        if matches!(self.fault.get(), Fault::Bound) {
            Ok(usize::MAX)
        } else {
            self.inner.read_buffer_bound(w, h, bands)
        }
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        let (mut raster, metrics) = self
            .inner
            .read_selected_window_cancellable(x, y, w, h, bands, bytes, cancel)?;
        match self.fault.get() {
            Fault::Shift => raster.grid.transform[0] += 1.,
            Fault::Mask => {
                raster.bands[0].valid.pop();
            }
            Fault::Bands => {
                raster.bands.pop();
            }
            Fault::Identity => raster.source_id.push_str("-changed"),
            Fault::Unit => raster.bands[0].unit = Some("wrong".into()),
            Fault::Nonfinite => {
                raster.bands[0].values[1] = f64::INFINITY;
                raster.bands[0].valid[1] = true;
            }
            _ => (),
        }
        Ok((raster, metrics))
    }
}
#[test]
fn adapter_window_contract_is_enforced_before_preparation_query_and_direct_reduction() {
    let source = Faulty {
        inner: procedural(),
        fault: Cell::new(Fault::None),
    };
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("valid");
    let cancel = AtomicBool::new(false);
    persistent::build_from_source(
        &source,
        index.to_str().unwrap(),
        16,
        "band_major",
        "hierarchy",
        BoundarySource::Original,
        &cancel,
    )
    .unwrap();
    let summary = index.join("summary.rsi");
    let polygon = json!({"type":"Polygon","coordinates":[[[1.25,1.25],[4.75,1.25],[4.75,4.75],[1.25,4.75],[1.25,1.25]]]});
    let options = Options::default();
    let query = || {
        persistent::query_with_boundary_source(
            QueryOptions {
                joint_planner: None,
                index_path: summary.to_str().unwrap(),
                raw_path: "",
                source_path: None,
                expected_build_id: None,
                read_memory_bytes: 1024 * 1024,
                summary_page_bytes: Some(0),
                coalesce_raw: true,
                order_summaries: false,
                forbidden_raw_tiles: &[],
                force_direct: false,
                hierarchy: None,
            },
            &polygon,
            "LOCAL",
            &options,
            &cancel,
            Some(&source),
        )
    };
    assert!(query().is_ok());
    for fault in [
        Fault::Shift,
        Fault::Mask,
        Fault::Bands,
        Fault::Identity,
        Fault::Unit,
        Fault::Nonfinite,
        Fault::Bound,
    ] {
        source.fault.set(fault);
        for mode in [BoundarySource::Original, BoundarySource::Normalized] {
            let destination = dir.path().join(format!(
                "invalid-{fault:?}-{}",
                if mode == BoundarySource::Original {
                    "original"
                } else {
                    "normalized"
                }
            ));
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                persistent::build_from_source(
                    &source,
                    destination.to_str().unwrap(),
                    16,
                    "band_major",
                    "hierarchy",
                    mode,
                    &cancel,
                )
            }));
            assert!(attempt.unwrap().is_err(), "build {fault:?}");
            assert!(
                !destination.exists(),
                "invalid index must never be published"
            );
        }
        let reads_before = source.inner.blocks.borrow().len();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(query));
        let error = attempt.unwrap().unwrap_err();
        if matches!(fault, Fault::Bound) {
            assert!(error.to_string().contains("memory bound overflow"));
            assert_eq!(source.inner.blocks.borrow().len(), reads_before);
        }
        let direct = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            streaming::measure_cached_source(
                &source,
                &polygon,
                "LOCAL",
                &options,
                &cancel,
                &mut TileCache::default(),
                0.,
            )
        }));
        assert!(direct.unwrap().is_err(), "direct {fault:?}");
    }
}
