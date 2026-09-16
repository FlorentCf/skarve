use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    Skarve,
    io::open_source,
    skv::{CompileOptions, compile},
    source_buffer::{Request, read},
};
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[test]
fn qualified_pixel_wide_raw_group_keeps_budget_masks_and_normalized_cap() {
    use gdal::raster::RasterCreationOptions;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel.tif");
    let opts = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=32",
        "BLOCKYSIZE=32",
        "INTERLEAVE=PIXEL",
        "COMPRESS=DEFLATE",
    ]);
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type_with_options::<f32, _>(&path, 64, 64, 40, &opts)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 64., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for id in 1..=40 {
        let mut band = ds.rasterband(id).unwrap();
        let mut values = (0..4096).map(|i| i as f32 + id as f32).collect::<Vec<_>>();
        values[65] = -0.;
        values[66] = -9999.;
        values[67] = f32::from_bits(0x7fc01234);
        band.write((0, 0), (64, 64), &mut Buffer::new((64, 64), values))
            .unwrap();
        band.set_no_data_value(Some(-9999.)).unwrap();
        band.set_scale(2.).unwrap();
        band.set_offset(7.).unwrap();
    }
    ds.build_overviews("NEAREST", &[2], &[]).unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let source = open_source(
        &serde_json::from_value(json!({"location":path})).unwrap(),
        &cancel,
    )
    .unwrap();
    let qualified = source.access_layout().get("raw_window_admission").is_some();
    assert_eq!(source.max_read_bands(), 20);
    assert_eq!(source.max_raw_read_bands(), if qualified { 64 } else { 20 });
    let selected = (0..40).rev().collect::<Vec<_>>();
    assert!(
        source.read_buffer_bound(16, 8, &selected).is_err(),
        "normalized cap is unchanged"
    );
    let request = Request {
        source: "x".into(),
        window: [1, 1, 16, 8],
        bands: selected,
        working_bytes: 128 << 20,
    };
    let mut bytes = vec![0; 40 * 16 * 8 * 5];
    let result = read(source.as_ref(), &request, &mut bytes, &cancel).unwrap();
    assert_eq!(result["rawReadGroups"], if qualified { 1 } else { 2 });
    assert_eq!(
        result["readMetrics"]["decoder_cache_flushes"],
        if qualified { 1 } else { 2 }
    );
    assert!(result["reservedBytes"]["working"].as_u64().unwrap() <= 128 << 20);
    for (position, &band) in request.bands.iter().enumerate() {
        let mut single = vec![0; 16 * 8 * 5];
        let reference = read(
            source.as_ref(),
            &Request {
                source: "x".into(),
                window: request.window,
                bands: vec![band],
                working_bytes: request.working_bytes,
            },
            &mut single,
            &cancel,
        )
        .unwrap();
        let descriptor = &result["bands"][position];
        let offset = descriptor["byteOffset"].as_u64().unwrap() as usize;
        let mask = descriptor["maskOffset"].as_u64().unwrap() as usize;
        assert_eq!(&bytes[offset..offset + 512], &single[..512]);
        assert_eq!(&bytes[mask..mask + 128], &single[512..640]);
        assert_eq!(descriptor["metadata"], reference["bands"][0]["metadata"]);
    }
    let tiny_limit =
        bytes.len() + (16 << 20) + source.raw_read_buffer_bound_at(1, 1, 16, 8, &[0]).unwrap() - 1;
    let before = source.diagnostics()["decoder_cache_flushes"].clone();
    assert!(
        read(
            source.as_ref(),
            &Request {
                working_bytes: tiny_limit,
                ..request
            },
            &mut bytes,
            &cancel
        )
        .is_err()
    );
    assert_eq!(
        source.diagnostics()["decoder_cache_flushes"],
        before,
        "budget rejection occurs before reading"
    );
    let overview = open_source(
        &serde_json::from_value(json!({"location":path,"overview":0})).unwrap(),
        &cancel,
    )
    .unwrap();
    assert_eq!(
        overview.max_raw_read_bands(),
        20,
        "unqualified overview keeps conservative width"
    );
}

#[test]
fn original_float_bits_masks_order_and_both_skv_layouts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&path, 9, 7, 40)
        .unwrap();
    ds.set_geo_transform(&[2., 0.5, 0., 10., 0., -0.5]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap())
        .unwrap();
    for b in 1..=40 {
        let mut band = ds.rasterband(b).unwrap();
        let mut values = (0..63).map(|i| i as f32 + b as f32).collect::<Vec<_>>();
        values[10] = f32::from_bits(0x7fc01234);
        values[11] = -0.;
        values[12] = -9999.;
        values[13] = f32::INFINITY;
        band.write((0, 0), (9, 7), &mut Buffer::new((9, 7), values))
            .unwrap();
        band.set_no_data_value(Some(-9999.)).unwrap();
        band.set_scale(2.).unwrap();
        band.set_offset(7.).unwrap();
    }
    ds.build_overviews("NEAREST", &[2], &[]).unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let source = open_source(
        &serde_json::from_value(json!({"location":path})).unwrap(),
        &cancel,
    )
    .unwrap();
    let request = Request {
        source: "x".into(),
        window: [1, 1, 5, 4],
        bands: vec![39, 0, 17],
        working_bytes: 128 << 20,
    };
    let mut baseline = vec![0; 512];
    let desc = read(source.as_ref(), &request, &mut baseline, &cancel).unwrap();
    assert_eq!(
        desc["bands"][0]["metadata"]["scaleBits"],
        "4000000000000000"
    );
    assert_eq!(
        u32::from_le_bytes(baseline[0..4].try_into().unwrap()),
        0x7fc01234
    );
    assert_eq!(
        u32::from_le_bytes(baseline[4..8].try_into().unwrap()),
        0x80000000
    );
    for layout in ["band", "row_group_v1"] {
        let output = dir.path().join(format!("{layout}.skv"));
        let options: CompileOptions =
            serde_json::from_value(json!({"chunk_edge":64,"band_group":4,"payload_layout":layout}))
                .unwrap();
        compile(source.as_ref(), output.to_str().unwrap(), &options, &cancel).unwrap();
        let mut engine = Skarve::new();
        let mut handle = engine.infuse(output.to_str().unwrap()).unwrap();
        let mut bytes = vec![0; 512];
        let result = handle
            .read_window(request.window, request.bands.clone(), &mut bytes, 128 << 20)
            .unwrap();
        assert_eq!(bytes, baseline);
        assert_eq!(result["bands"], desc["bands"]);
        assert_eq!(
            handle.inspect().unwrap()["rawMetadata"]["maxWindowBands"],
            64
        );
        for count in [37, 40] {
            let selected = (0..count).rev().collect::<Vec<_>>();
            let mut complete = vec![0; 8192];
            let result = handle
                .read_window(request.window, selected.clone(), &mut complete, 128 << 20)
                .unwrap();
            for (position, &band) in selected.iter().enumerate() {
                let mut single = vec![0; 512];
                let reference = handle
                    .read_window(request.window, vec![band], &mut single, 128 << 20)
                    .unwrap();
                let descriptor = &result["bands"][position];
                assert_eq!(descriptor["sourceBand"], band);
                assert_eq!(descriptor["metadata"], reference["bands"][0]["metadata"]);
                let offset = descriptor["byteOffset"].as_u64().unwrap() as usize;
                let mask = descriptor["maskOffset"].as_u64().unwrap() as usize;
                assert_eq!(&complete[offset..offset + 80], &single[..80]);
                assert_eq!(&complete[mask..mask + 20], &single[80..100]);
            }
        }
        assert!(
            handle
                .read_window(request.window, request.bands.clone(), &mut bytes, 512)
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        assert!(
            handle
                .read_window([8, 0, 2, 1], vec![0], &mut bytes, 128 << 20)
                .is_err()
        );
    }
    let overview = open_source(
        &serde_json::from_value(json!({"location":path,"overview":0})).unwrap(),
        &cancel,
    )
    .unwrap();
    assert_eq!(overview.metadata().grid.width, 5);
    let mut bytes = vec![0; 512];
    let result = read(
        overview.as_ref(),
        &Request {
            window: [0, 0, 5, 4],
            bands: vec![0],
            ..request
        },
        &mut bytes,
        &cancel,
    )
    .unwrap();
    assert_eq!(result["width"], 5);
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        read(
            source.as_ref(),
            &Request {
                source: "x".into(),
                window: [0, 0, 1, 1],
                bands: vec![0],
                working_bytes: 128 << 20
            },
            &mut bytes,
            &cancel
        )
        .unwrap_err()
        .to_string()
        .contains("cancel")
    );
}

#[test]
fn grouped_reads_preflight_caps_and_verify_only_the_complete_operation() {
    use raster_engine::{model::*, source::*};
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::Ordering;
    struct Grouped {
        metadata: RasterMetadata,
        raw: RawRasterMetadata,
        reads: RefCell<Vec<Vec<usize>>>,
        verifies: Cell<usize>,
        reject_band: Cell<Option<usize>>,
        cancel_on_read: Cell<bool>,
        corrupt_on_read: Cell<bool>,
        fail_final_verify: Cell<bool>,
    }
    impl WindowSource for Grouped {
        fn metadata(&self) -> &RasterMetadata {
            &self.metadata
        }
        fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
            Some(&self.raw)
        }
        fn verify_immutable(&self) -> anyhow::Result<()> {
            self.verifies.set(self.verifies.get() + 1);
            anyhow::ensure!(
                !self.fail_final_verify.get() || self.verifies.get() != 2,
                "source generation changed"
            );
            Ok(())
        }
        fn raw_read_buffer_bound_at(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            bands: &[usize],
        ) -> anyhow::Result<usize> {
            assert_eq!((x, y, w, h), (1, 1, 1, 1));
            assert!(bands.len() <= self.max_read_bands());
            Ok(
                if self.reject_band.get().is_some_and(|b| bands.contains(&b)) {
                    128 << 20
                } else {
                    bands.len() * 1024
                },
            )
        }
        fn read_raw_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            w: usize,
            h: usize,
            bands: &[usize],
            budget: usize,
            cancel: &AtomicBool,
        ) -> anyhow::Result<(RawWindow, ReadMetrics)> {
            assert_eq!(budget, bands.len() * 1024);
            self.reads.borrow_mut().push(bands.to_vec());
            anyhow::ensure!(!self.corrupt_on_read.get(), "raw checksum mismatch");
            if self.cancel_on_read.get() {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok((
                RawWindow {
                    width: w,
                    height: h,
                    bands: bands
                        .iter()
                        .map(|&b| RawBandWindow {
                            samples_le: (0x7fc01000u32 + b as u32).to_le_bytes().to_vec(),
                            mask: vec![if b % 2 == 0 { 0 } else { 255 }],
                        })
                        .collect(),
                },
                ReadMetrics {
                    raster_io_calls: bands.len() * 2,
                    ..ReadMetrics::default()
                },
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
        ) -> anyhow::Result<(Raster, ReadMetrics)> {
            anyhow::bail!("normalization must not be called")
        }
    }
    let source = Grouped {
        metadata: RasterMetadata {
            grid: Grid {
                width: 2,
                height: 2,
                transform: [0., 1., 0., 2., 0., -1.],
                crs: "EPSG:3857".into(),
            },
            bands: (0..40)
                .map(|_| BandMetadata {
                    data_type: "Float32".into(),
                    nodata: None,
                    scale: 2.,
                    offset: 7.,
                    unit: None,
                    block_size: (1, 1),
                })
                .collect(),
            source_id: "grouped".into(),
        },
        raw: RawRasterMetadata {
            bands: (0..40)
                .map(|b| RawBandMetadata {
                    scalar_type: RawScalarType::Float32,
                    nodata_f64_bits: None,
                    scale_f64_bits: 2f64.to_bits(),
                    offset_f64_bits: 7f64.to_bits(),
                    unit: None,
                    mask_flags: 0,
                    original_band_index: b,
                    description: b.to_string(),
                })
                .collect(),
            pixel_convention: "area".into(),
            source_band_count: 40,
            source_overview: None,
        },
        reads: RefCell::new(Vec::new()),
        verifies: Cell::new(0),
        reject_band: Cell::new(None),
        cancel_on_read: Cell::new(false),
        corrupt_on_read: Cell::new(false),
        fail_final_verify: Cell::new(false),
    };
    let cancel = AtomicBool::new(false);
    let mut output = vec![0; 320];
    let mut request = Request {
        source: "grouped".into(),
        window: [1, 1, 1, 1],
        bands: (0..40).rev().collect(),
        working_bytes: (16 << 20) + output.len() + 3 * 1024,
    };
    let result = read(&source, &request, &mut output, &cancel).unwrap();
    assert_eq!(source.verifies.get(), 2);
    assert_eq!(result["rawReadGroups"], 14);
    assert_eq!(result["readMetrics"]["raster_io_calls"], 80);
    assert!(source.reads.borrow().iter().all(|bands| bands.len() <= 3));
    assert_eq!(source.reads.borrow().concat(), request.bands);
    assert_eq!(result["reservedBytes"]["working"], request.working_bytes);
    for (position, &b) in request.bands.iter().enumerate() {
        assert_eq!(
            &output[position * 8..position * 8 + 4],
            &(0x7fc01000u32 + b as u32).to_le_bytes()
        );
        assert_eq!(output[position * 8 + 4], if b % 2 == 0 { 0 } else { 255 });
    }
    source.reads.borrow_mut().clear();
    source.verifies.set(0);
    // The final group's admission fails before any source access.
    source.reject_band.set(Some(0));
    assert!(
        read(&source, &request, &mut output, &cancel)
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
    assert!(source.reads.borrow().is_empty());
    assert_eq!(source.verifies.get(), 0);
    source.reject_band.set(None);
    request.bands = vec![0, 0];
    assert!(read(&source, &request, &mut output, &cancel).is_err());
    assert!(source.reads.borrow().is_empty());
    assert_eq!(source.verifies.get(), 0);
    request.bands = (0..37).collect();
    source.cancel_on_read.set(true);
    assert!(
        read(&source, &request, &mut output, &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    assert_eq!(source.reads.borrow().len(), 1);
    cancel.store(false, Ordering::Relaxed);
    source.cancel_on_read.set(false);
    source.reads.borrow_mut().clear();
    source.verifies.set(0);
    source.corrupt_on_read.set(true);
    assert!(
        read(&source, &request, &mut output, &cancel)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    assert_eq!(source.reads.borrow().len(), 1);
    source.corrupt_on_read.set(false);
    source.reads.borrow_mut().clear();
    source.verifies.set(0);
    source.fail_final_verify.set(true);
    assert!(
        read(&source, &request, &mut output, &cancel)
            .unwrap_err()
            .to_string()
            .contains("generation")
    );
    assert_eq!(source.verifies.get(), 2);
    assert_eq!(source.reads.borrow().concat(), request.bands);
}

#[test]
fn integer_samples_are_not_converted_to_float() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("integer.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<u32, _>(&path, 3, 1, 1)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 1., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    ds.rasterband(1)
        .unwrap()
        .write(
            (0, 0),
            (3, 1),
            &mut Buffer::new((3, 1), vec![u32::MAX, 16_777_217, 0]),
        )
        .unwrap();
    drop(ds);
    let mut engine = Skarve::new();
    let mut source = engine.infuse(path.to_str().unwrap()).unwrap();
    let mut bytes = vec![0; 15];
    let result = source
        .read_window([0, 0, 3, 1], vec![0], &mut bytes, 128 << 20)
        .unwrap();
    assert_eq!(result["bands"][0]["scalarType"], "uint32");
    assert_eq!(&bytes[0..4], &u32::MAX.to_le_bytes());
    assert_eq!(&bytes[4..8], &16_777_217u32.to_le_bytes());
    assert_eq!(&bytes[12..], &[255; 3]);
    std::fs::write(&path, b"changed").unwrap();
    assert!(
        source
            .read_window([0, 0, 3, 1], vec![0], &mut bytes, 128 << 20)
            .is_err()
    );
}

#[test]
fn mixed_scalar_descriptor_alignment_preserves_every_byte() {
    use raster_engine::{model::*, source::*};
    struct Mixed {
        metadata: RasterMetadata,
        raw: RawRasterMetadata,
    }
    impl WindowSource for Mixed {
        fn metadata(&self) -> &RasterMetadata {
            &self.metadata
        }
        fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
            Some(&self.raw)
        }
        fn verify_immutable(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn raw_read_buffer_bound(&self, _: usize, _: usize, _: &[usize]) -> anyhow::Result<usize> {
            Ok(1024)
        }
        fn read_raw_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            w: usize,
            h: usize,
            indices: &[usize],
            _: usize,
            _: &AtomicBool,
        ) -> anyhow::Result<(RawWindow, ReadMetrics)> {
            let payloads = [
                vec![128u8],
                u32::MAX.to_le_bytes().to_vec(),
                0x7ff8000000001234u64.to_le_bytes().to_vec(),
            ];
            Ok((
                RawWindow {
                    width: w,
                    height: h,
                    bands: indices
                        .iter()
                        .map(|&i| RawBandWindow {
                            samples_le: payloads[i].clone(),
                            mask: vec![if i == 1 { 0 } else { 127 }],
                        })
                        .collect(),
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
        ) -> anyhow::Result<(Raster, ReadMetrics)> {
            anyhow::bail!("normalization must not be called")
        }
    }
    let types = [
        RawScalarType::Int8,
        RawScalarType::UInt32,
        RawScalarType::Float64,
    ];
    let source = Mixed {
        metadata: RasterMetadata {
            grid: Grid {
                width: 1,
                height: 1,
                transform: [0., 1., 0., 1., 0., -1.],
                crs: "EPSG:3857".into(),
            },
            bands: types
                .iter()
                .map(|_| BandMetadata {
                    data_type: "fixture".into(),
                    nodata: None,
                    scale: 1.,
                    offset: 0.,
                    unit: None,
                    block_size: (1, 1),
                })
                .collect(),
            source_id: "mixed-fixture".into(),
        },
        raw: RawRasterMetadata {
            bands: types
                .iter()
                .enumerate()
                .map(|(i, &scalar_type)| RawBandMetadata {
                    scalar_type,
                    nodata_f64_bits: None,
                    scale_f64_bits: 1f64.to_bits(),
                    offset_f64_bits: 0f64.to_bits(),
                    unit: None,
                    mask_flags: 0,
                    original_band_index: i,
                    description: "".into(),
                })
                .collect(),
            pixel_convention: "area".into(),
            source_band_count: 3,
            source_overview: None,
        },
    };
    let mut output = vec![0; 32];
    let result = read(
        &source,
        &Request {
            source: "mixed".into(),
            window: [0, 0, 1, 1],
            bands: vec![2, 0, 1],
            working_bytes: 128 << 20,
        },
        &mut output,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(&output[..8], &0x7ff8000000001234u64.to_le_bytes());
    assert_eq!(output[8], 127);
    assert_eq!(result["bands"][1]["byteOffset"], 16);
    assert_eq!(output[16], 128);
    assert_eq!(output[17], 127);
    assert_eq!(result["bands"][2]["byteOffset"], 24);
    assert_eq!(&output[24..28], &u32::MAX.to_le_bytes());
    assert_eq!(output[28], 0);
}
