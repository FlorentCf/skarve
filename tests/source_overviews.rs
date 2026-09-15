use gdal::{DriverManager, Metadata, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{io::open_source, source::SourceSpec};
use serde_json::json;
use std::{path::Path, sync::atomic::AtomicBool};

fn fixture(path: &Path, levels: bool) {
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(path, 8, 8, 2)
        .unwrap();
    ds.set_geo_transform(&[2., 0.25, 0., 52., 0., -0.25])
        .unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap())
        .unwrap();
    for band in 1..=2 {
        ds.rasterband(band)
            .unwrap()
            .set_no_data_value(Some(-99999.))
            .unwrap();
        ds.rasterband(band).unwrap().set_scale(2.).unwrap();
        ds.rasterband(band).unwrap().set_offset(-3.).unwrap();
        ds.rasterband(band)
            .unwrap()
            .write(
                (0, 0),
                (8, 8),
                &mut Buffer::new((8, 8), vec![band as f32; 64]),
            )
            .unwrap();
    }
    if levels {
        ds.build_overviews("NEAREST", &[2, 4], &[]).unwrap();
        // Existing overview samples intentionally differ from any resampling
        // of the native constant raster: selection must read these exact bits.
        for band in 1..=2 {
            let parent = ds.rasterband(band).unwrap();
            parent
                .overview(0)
                .unwrap()
                .write(
                    (0, 0),
                    (4, 4),
                    &mut Buffer::new((4, 4), (0..16).map(|i| (100 * band + i) as f32).collect()),
                )
                .unwrap();
            parent
                .overview(1)
                .unwrap()
                .write(
                    (0, 0),
                    (2, 2),
                    &mut Buffer::new((2, 2), vec![(1000 * band) as f32; 4]),
                )
                .unwrap();
        }
    }
    ds.flush_cache().unwrap();
}

fn spec(value: serde_json::Value) -> SourceSpec {
    serde_json::from_value(value).unwrap()
}

#[test]
fn explicit_existing_overview_preserves_bits_grid_mapping_and_skv_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("views.tif");
    fixture(&path, true);
    let cancel = AtomicBool::new(false);
    let native = open_source(&spec(json!({"location":path})), &cancel).unwrap();
    let view = open_source(
        &spec(json!({"location":path,"overview":0,"bands":[1,0]})),
        &cancel,
    )
    .unwrap();
    assert_eq!(native.metadata().grid.width, 8);
    assert_eq!(view.metadata().grid.width, 4);
    assert_eq!(view.metadata().grid.height, 4);
    assert_eq!(view.metadata().grid.transform, [2., 0.5, 0., 52., 0., -0.5]);
    assert_ne!(view.metadata().source_id, native.metadata().source_id);
    assert_eq!(view.raw_metadata().unwrap().source_overview, Some(0));
    assert_eq!(view.raw_metadata().unwrap().bands[0].original_band_index, 1);
    for metadata in &view.metadata().bands {
        assert_eq!(metadata.nodata, Some(-99999.));
        assert_eq!(metadata.scale, 2.);
        assert_eq!(metadata.offset, -3.);
    }
    let bound = view.raw_read_buffer_bound(4, 4, &[0, 1]).unwrap();
    let (raw, _) = view
        .read_raw_selected_window_cancellable(0, 0, 4, 4, &[0, 1], bound, &cancel)
        .unwrap();
    for (b, start) in [200, 100].into_iter().enumerate() {
        let expected: Vec<_> = (start..start + 16)
            .flat_map(|n| (n as f32).to_le_bytes())
            .collect();
        assert_eq!(raw.bands[b].samples_le, expected);
    }
    let skv = dir.path().join("view.skv");
    raster_engine::skv::compile(
        view.as_ref(),
        skv.to_str().unwrap(),
        &raster_engine::skv::CompileOptions {
            predictor: "byte_delta_v1".into(),
            ..Default::default()
        },
        &cancel,
    )
    .unwrap();
    let metadata = serde_json::to_value(view.raw_metadata()).unwrap();
    let grid = serde_json::to_value(&view.metadata().grid).unwrap();
    drop(view);
    drop(native);
    std::fs::rename(&path, dir.path().join("original-unavailable.tif")).unwrap();
    let serving = open_source(&spec(json!({"location":skv})), &cancel).unwrap();
    assert_eq!(
        serde_json::to_value(serving.raw_metadata()).unwrap(),
        metadata
    );
    assert_eq!(
        serde_json::to_value(&serving.metadata().grid).unwrap(),
        grid
    );
    let bound = serving.raw_read_buffer_bound(4, 4, &[0, 1]).unwrap();
    let (decoded, _) = serving
        .read_raw_selected_window_cancellable(0, 0, 4, 4, &[0, 1], bound, &cancel)
        .unwrap();
    for b in 0..2 {
        assert_eq!(decoded.bands[b].samples_le, raw.bands[b].samples_le);
        assert_eq!(decoded.bands[b].mask, raw.bands[b].mask);
    }
    assert!(open_source(&spec(json!({"location":skv,"overview":0})), &cancel).is_err());
}

#[test]
fn unavailable_or_external_overview_rejects_and_generation_includes_new_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("views.tif");
    fixture(&path, true);
    let cancel = AtomicBool::new(false);
    for level in [2, 63, 64] {
        assert!(open_source(&spec(json!({"location":path,"overview":level})), &cancel).is_err());
    }
    let plain = dir.path().join("plain.tif");
    fixture(&plain, false);
    assert!(open_source(&spec(json!({"location":plain,"overview":0})), &cancel).is_err());
    let source = open_source(&spec(json!({"location":path,"overview":0})), &cancel).unwrap();
    std::fs::write(
        format!("{}.ovr", path.display()),
        b"external-overviews-unsupported",
    )
    .unwrap();
    assert!(source.verify_immutable().is_err());
    assert!(open_source(&spec(json!({"location":path,"overview":0})), &cancel).is_err());
}

#[test]
fn explicitly_conflicting_overview_scale_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conflicting.tif");
    fixture(&path, true);
    let mut ds = gdal::Dataset::open_ex(
        &path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_RASTER | gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..Default::default()
        },
    )
    .unwrap();
    ds.rasterband(1)
        .unwrap()
        .overview(0)
        .unwrap()
        .set_scale(9.)
        .unwrap();
    ds.flush_cache().unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let error = open_source(&spec(json!({"location":path,"overview":0})), &cancel)
        .err()
        .unwrap();
    assert!(error.to_string().contains("conflicting scale"), "{error}");
}

#[test]
fn selected_existing_overview_preserves_stored_internal_mask_and_nodata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("masked-view.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&path, 8, 8, 1)
        .unwrap();
    ds.set_geo_transform(&[2., 0.25, 0., 52., 0., -0.25])
        .unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap())
        .unwrap();
    {
        let mut band = ds.rasterband(1).unwrap();
        band.set_no_data_value(Some(-99999.)).unwrap();
        band.set_scale(2.).unwrap();
        band.set_offset(-3.).unwrap();
        band.write((0, 0), (8, 8), &mut Buffer::new((8, 8), vec![7.; 64]))
            .unwrap();
        // Force an internal, per-dataset mask even on older GDAL defaults.
        let key = "GDAL_TIFF_INTERNAL_MASK";
        let previous = gdal::config::get_thread_local_config_option(key, "").unwrap();
        gdal::config::set_thread_local_config_option(key, "YES").unwrap();
        let created = band.create_mask_band(true);
        if previous.is_empty() {
            gdal::config::clear_thread_local_config_option(key).unwrap();
        } else {
            gdal::config::set_thread_local_config_option(key, &previous).unwrap();
        }
        created.unwrap();
        band.open_mask_band()
            .unwrap()
            .write((0, 0), (8, 8), &mut Buffer::new((8, 8), vec![255u8; 64]))
            .unwrap();
    }
    ds.build_overviews("NEAREST", &[2], &[]).unwrap();
    let mut values = (1..=16).map(|n| n as f32).collect::<Vec<_>>();
    values[1] = -99999.; // Explicit NoData, with a present independent mask.
    values[2] = 99.; // Finite, non-NoData sample excluded only by the stored mask.
    values[3] = f32::from_bits(0x7fc0_1234); // Preserve nonfinite payload bits.
    let mut masks = vec![255u8; 16];
    masks[2] = 0;
    {
        let parent = ds.rasterband(1).unwrap();
        let mut overview = parent.overview(0).unwrap();
        overview
            .write((0, 0), (4, 4), &mut Buffer::new((4, 4), values.clone()))
            .unwrap();
        let mut mask = overview.open_mask_band().unwrap();
        assert_eq!(
            mask.size(),
            (4, 4),
            "an existing stored overview mask is required"
        );
        mask.write((0, 0), (4, 4), &mut Buffer::new((4, 4), masks.clone()))
            .unwrap();
    }
    ds.flush_cache().unwrap();
    drop(ds);
    assert!(!Path::new(&format!("{}.msk", path.display())).exists());
    assert!(!Path::new(&format!("{}.ovr", path.display())).exists());
    // Independently reopen the existing band/overview rather than the adapter's
    // OVERVIEW_LEVEL proxy, and verify the exact fixture's stored samples/mask.
    let original = gdal::Dataset::open(&path).unwrap();
    let parent = original.rasterband(1).unwrap();
    let overview = parent.overview(0).unwrap();
    let direct_values = overview
        .read_as::<f32>((0, 0), (4, 4), (4, 4), None)
        .unwrap();
    assert_eq!(
        direct_values
            .data()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        values.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
        overview
            .open_mask_band()
            .unwrap()
            .read_as::<u8>((0, 0), (4, 4), (4, 4), None)
            .unwrap()
            .data(),
        masks
    );
    drop(original);

    let cancel = AtomicBool::new(false);
    let view = open_source(&spec(json!({"location":path,"overview":0})), &cancel).unwrap();
    let expected = values
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    let (raw, _) = view
        .read_raw_selected_window_cancellable(0, 0, 4, 4, &[0], 16 << 20, &cancel)
        .unwrap();
    assert_eq!(raw.bands[0].samples_le, expected);
    assert_eq!(raw.bands[0].mask, masks);
    assert_eq!(
        view.raw_metadata().unwrap().bands[0].nodata_f64_bits,
        Some((-99999f64).to_bits())
    );
    let (normal, _) = view
        .read_selected_window_cancellable(0, 0, 4, 4, &[0], 16 << 20, &cancel)
        .unwrap();
    let valid = (0..16).map(|i| ![1, 2, 3].contains(&i)).collect::<Vec<_>>();
    let normalized = values
        .iter()
        .enumerate()
        .map(|(i, &v)| if valid[i] { (v as f64) * 2. - 3. } else { 0. })
        .collect::<Vec<_>>();
    assert_eq!(normal.bands[0].valid, valid);
    assert_eq!(normal.bands[0].values, normalized);

    let output = dir.path().join("masked-view.skv");
    raster_engine::skv::compile(
        view.as_ref(),
        output.to_str().unwrap(),
        &raster_engine::skv::CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    drop(view);
    std::fs::rename(&path, dir.path().join("original-unavailable.tif")).unwrap();
    let serving = open_source(&spec(json!({"location":output})), &cancel).unwrap();
    let (round_trip, _) = serving
        .read_raw_selected_window_cancellable(0, 0, 4, 4, &[0], 16 << 20, &cancel)
        .unwrap();
    assert_eq!(round_trip.bands[0].samples_le, expected);
    assert_eq!(round_trip.bands[0].mask, masks);
    let (normal, _) = serving
        .read_selected_window_cancellable(0, 0, 4, 4, &[0], 16 << 20, &cancel)
        .unwrap();
    assert_eq!(normal.bands[0].valid, valid);
    assert_eq!(normal.bands[0].values, normalized);
}

#[test]
fn selected_pixel_is_point_overview_preserves_units_and_registration_through_skv() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("point-view.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&path, 8, 8, 2)
        .unwrap();
    ds.set_geo_transform(&[2., 0.25, 0., 52., 0., -0.25])
        .unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap())
        .unwrap();
    ds.set_metadata_item("AREA_OR_POINT", "Point", "").unwrap();
    for (index, name) in ["people", "people per cell"].into_iter().enumerate() {
        let mut band = ds.rasterband(index + 1).unwrap();
        let unit = std::ffi::CString::new(name).unwrap();
        assert_eq!(
            unsafe { gdal_sys::GDALSetRasterUnitType(band.c_rasterband(), unit.as_ptr()) },
            0
        );
        band.write(
            (0, 0),
            (8, 8),
            &mut Buffer::new((8, 8), vec![(index + 1) as f32; 64]),
        )
        .unwrap();
    }
    ds.build_overviews("NEAREST", &[2], &[]).unwrap();
    ds.flush_cache().unwrap();
    drop(ds);

    let cancel = AtomicBool::new(false);
    let native = open_source(&spec(json!({"location":path})), &cancel).unwrap();
    assert_eq!(native.raw_metadata().unwrap().pixel_convention, "Point");
    let view = open_source(
        &spec(json!({"location":path,"overview":0,"bands":[1,0]})),
        &cancel,
    )
    .unwrap();
    assert_eq!(view.metadata().grid.transform, [2., 0.5, 0., 52., 0., -0.5]);
    assert_eq!(view.raw_metadata().unwrap().pixel_convention, "Point");
    assert_eq!(view.access_layout()["registration"], "Point");
    assert_ne!(view.metadata().source_id, native.metadata().source_id);
    for (index, unit) in ["people per cell", "people"].into_iter().enumerate() {
        assert_eq!(view.metadata().bands[index].unit.as_deref(), Some(unit));
        assert_eq!(
            view.raw_metadata().unwrap().bands[index].unit.as_deref(),
            Some(unit)
        );
        assert_eq!(
            view.raw_metadata().unwrap().bands[index].original_band_index,
            1 - index
        );
    }
    let raw_metadata = serde_json::to_value(view.raw_metadata().unwrap()).unwrap();
    let grid = serde_json::to_value(&view.metadata().grid).unwrap();
    let output = dir.path().join("point-view.skv");
    raster_engine::skv::compile(
        view.as_ref(),
        output.to_str().unwrap(),
        &raster_engine::skv::CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    drop(view);
    drop(native);
    std::fs::rename(&path, dir.path().join("original-unavailable.tif")).unwrap();
    let serving = open_source(&spec(json!({"location":output})), &cancel).unwrap();
    assert_eq!(
        serde_json::to_value(serving.raw_metadata().unwrap()).unwrap(),
        raw_metadata
    );
    assert_eq!(
        serde_json::to_value(&serving.metadata().grid).unwrap(),
        grid
    );
    let (raw, _) = serving
        .read_raw_selected_window_cancellable(0, 0, 4, 4, &[0, 1], 16 << 20, &cancel)
        .unwrap();
    for (index, value) in [2f32, 1.].into_iter().enumerate() {
        assert_eq!(
            raw.bands[index].samples_le,
            (0..16)
                .flat_map(|_| value.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(raw.bands[index].mask, vec![255; 16]);
    }
}
