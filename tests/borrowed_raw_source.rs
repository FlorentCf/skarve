//! Retained source IDs use BorrowedSource in the exactextract batch path.
//! The wrapper must preserve typed capability, original values and read controls.
#![cfg(feature = "exactextract")]

use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    backend::{self, EXACTEXTRACT_RASTERIO_POLICY},
    batch::BorrowedSource,
    exactextract::{self, Input, Output},
    model::{Grid, Raster, check_cancel},
    source::{
        BandMetadata, RasterMetadata, RawBandMetadata, RawBandWindow, RawRasterMetadata,
        RawScalarType, RawWindow, ReadMetrics, WindowSource,
    },
    tile_cache::TileCache,
};
use serde_json::json;
use std::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

struct TypedSource {
    meta: RasterMetadata,
    raw: RawRasterMetadata,
    reads: Cell<usize>,
    reservation: Cell<usize>,
    cancel_after_read: Cell<bool>,
}
impl TypedSource {
    fn new(count: usize) -> Self {
        Self {
            meta: RasterMetadata {
                grid: Grid {
                    width: 3,
                    height: 1,
                    transform: [0., 1., 0., 1., 0., -1.],
                    crs: "LOCAL".into(),
                },
                bands: (0..count)
                    .map(|_| BandMetadata {
                        data_type: "Float32".into(),
                        nodata: Some(-9999.),
                        scale: 1.,
                        offset: 0.,
                        unit: Some("people".into()),
                        block_size: (3, 1),
                    })
                    .collect(),
                source_id: format!("retained-typed-{count}"),
            },
            raw: RawRasterMetadata {
                bands: (0..count)
                    .map(|i| RawBandMetadata {
                        scalar_type: RawScalarType::Float32,
                        nodata_f64_bits: Some((-9999_f64).to_bits()),
                        scale_f64_bits: 1_f64.to_bits(),
                        offset_f64_bits: 0,
                        unit: Some("people".into()),
                        mask_flags: 8,
                        original_band_index: i,
                        description: format!("band-{i}"),
                    })
                    .collect(),
                pixel_convention: "Area".into(),
                source_band_count: count,
                source_overview: None,
            },
            reads: Cell::new(0),
            reservation: Cell::new(4096),
            cancel_after_read: Cell::new(false),
        }
    }
}
impl WindowSource for TypedSource {
    fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        Some(&self.raw)
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        ensure!(
            !bands.is_empty() && bands.iter().all(|&b| b < self.raw.bands.len()),
            "bad typed selection"
        );
        Ok(self.reservation.get() + w * h * bands.len() * 5)
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
            self.raw_read_buffer_bound(w, h, bands)? <= max,
            "typed source budget exceeded"
        );
        ensure!(y == 0 && h == 1 && x + w <= 3, "bad typed window");
        self.reads.set(self.reads.get() + 1);
        let bands = bands
            .iter()
            .map(|&b| {
                let v = (b + 1) as f32;
                let values = [v, -9999., 3. * v];
                RawBandWindow {
                    samples_le: values[x..x + w]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect(),
                    mask: [255, 0, 255][x..x + w].to_vec(),
                }
            })
            .collect();
        if self.cancel_after_read.get() {
            cancel.store(true, Ordering::Relaxed);
        }
        Ok((
            RawWindow {
                width: w,
                height: h,
                bands,
            },
            ReadMetrics::default(),
        ))
    }
    fn read_selected_window_cancellable(
        &self,
        _: usize,
        _: usize,
        _: usize,
        _: usize,
        _: &[usize],
        _: usize,
        _: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        anyhow::bail!("normalized fallback must not run for typed compatibility")
    }
}
fn run(
    source: &dyn WindowSource,
    count: usize,
    strategy: &str,
    cancel: &AtomicBool,
) -> Result<Output> {
    let selection = backend::resolve(
        &json!({
            "backend": "exactextract", "numerical_policy": EXACTEXTRACT_RASTERIO_POLICY,
            "backend_options": {"strategy": strategy, "max_cells_in_memory": 3, "window_bytes": 64 << 20}
        }),
        false,
    )?;
    exactextract::execute(
        &[Input {
            source,
            bands: (0..count).rev().collect(),
        }],
        &[
            json!({"type":"Polygon","coordinates":[[[0,0],[3,0],[3,1],[0,1],[0,0]]]}),
            json!({"type":"Polygon","coordinates":[[[2,0],[3,0],[3,1],[2,1],[2,0]]]}),
        ],
        "LOCAL",
        &Options::default(),
        &selection.options,
        cancel,
        &mut TileCache::default(),
        512 << 20,
    )
}

#[test]
fn borrowed_typed_source_reaches_actual_ee_for_36_and_40_permuted_bands() {
    for count in [36, 40] {
        for strategy in ["feature-sequential", "raster-sequential"] {
            let source = TypedSource::new(count);
            let borrowed = BorrowedSource(&source);
            assert!(std::ptr::eq(borrowed.raw_metadata().unwrap(), &source.raw));
            let cancel = AtomicBool::new(false);
            let direct = run(&source, count, strategy, &cancel).unwrap();
            source.reads.set(0);
            let actual = run(&borrowed, count, strategy, &cancel).unwrap();
            assert!(source.reads.get() >= count);
            assert_eq!(actual.band_count, count);
            assert_eq!(
                actual.descriptors[0].bands,
                (0..count).rev().collect::<Vec<_>>()
            );
            assert_eq!(actual.defined, direct.defined);
            assert_eq!(
                actual
                    .values
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                direct
                    .values
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>()
            );
            for (i, band) in (0..count).rev().enumerate() {
                let v = (band + 1) as f64;
                assert_eq!(
                    &actual.values[i * 5..i * 5 + 5],
                    &[4. * v, 2., 2. * v, v, 3. * v]
                );
                let start = (count + i) * 5;
                assert_eq!(
                    &actual.values[start..start + 5],
                    &[3. * v, 1., 3. * v, 3. * v, 3. * v]
                );
            }
        }
    }
}

#[test]
fn borrowed_raw_bounds_and_cancellation_reject_without_completed_output() {
    for strategy in ["feature-sequential", "raster-sequential"] {
        let source = TypedSource::new(40);
        let borrowed = BorrowedSource(&source);
        let cancel = AtomicBool::new(false);
        let bound = borrowed.raw_read_buffer_bound(3, 1, &[39, 0]).unwrap();
        assert_eq!(bound, source.raw_read_buffer_bound(3, 1, &[39, 0]).unwrap());
        let error = borrowed
            .read_raw_selected_window_cancellable(0, 0, 3, 1, &[39, 0], bound - 1, &cancel)
            .err()
            .unwrap();
        assert!(error.to_string().contains("typed source budget exceeded"));
        assert_eq!(source.reads.get(), 0);
        // The actual EE callback must charge the advertised typed reserve before reading.
        source.reservation.set(64 << 20);
        let error = run(&borrowed, 40, strategy, &cancel).err().unwrap();
        assert!(format!("{error:#}").contains("window_bytes"));
        assert_eq!(source.reads.get(), 0);
        source.reservation.set(4096);
        cancel.store(true, Ordering::Relaxed);
        assert!(run(&borrowed, 40, strategy, &cancel).is_err());
        assert_eq!(source.reads.get(), 0);
        cancel.store(false, Ordering::Relaxed);
        source.cancel_after_read.set(true);
        assert!(run(&borrowed, 40, strategy, &cancel).is_err());
        assert!(
            cancel.load(Ordering::Relaxed),
            "caller cancellation token was not forwarded"
        );
        assert_eq!(source.reads.get(), 1);
        source.cancel_after_read.set(false);
        cancel.store(false, Ordering::Relaxed);
        assert!(run(&borrowed, 40, strategy, &cancel).is_ok());
    }
}
