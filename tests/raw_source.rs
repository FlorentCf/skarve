use gdal::{
    DriverManager, Metadata,
    raster::{Buffer, GdalType},
    spatial_ref::SpatialRef,
};
use raster_engine::io::{open_source, open_source_for_compile};
use raster_engine::source::{RawRasterMetadata, RawScalarType, SourceSpec};
use std::{io::Write, path::Path, sync::atomic::AtomicBool};

fn fixture<T: GdalType + Copy>(path: &Path, values: Vec<T>, bands: usize) {
    let mut dataset = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<T, _>(path, 4, 2, bands)
        .unwrap();
    dataset
        .set_geo_transform(&[7., 2., 0., 11., 0., -2.])
        .unwrap();
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    dataset
        .set_metadata_item("AREA_OR_POINT", "Point", "")
        .unwrap();
    for i in 1..=bands {
        let mut band = dataset.rasterband(i).unwrap();
        band.write((0, 0), (4, 2), &mut Buffer::new((4, 2), values.clone()))
            .unwrap();
        band.set_description(&format!("logical-band-{i}")).unwrap();
        band.set_scale(2.).unwrap();
        band.set_offset(-0.).unwrap();
        band.set_no_data_value(Some(-0.)).unwrap();
        let unit = std::ffi::CString::new("test unit").unwrap();
        assert_eq!(
            unsafe { gdal_sys::GDALSetRasterUnitType(band.c_rasterband(), unit.as_ptr()) },
            0
        );
        band.create_mask_band(false).unwrap();
        band.open_mask_band()
            .unwrap()
            .write(
                (0, 0),
                (4, 2),
                &mut Buffer::new((4, 2), vec![255u8, 127, 0, 1, 255, 0, 254, 255]),
            )
            .unwrap();
    }
    dataset.flush_cache().unwrap();
}
fn spec(path: &Path) -> SourceSpec {
    serde_json::from_value(serde_json::json!({"location": path})).unwrap()
}

#[test]
fn raw_all_admitted_types_preserve_payload_bits_masks_and_interpretation() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    macro_rules! check {
        ($name:literal, $kind:expr, $values:expr) => {{
            let values = $values;
            let expected = values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>();
            let path = dir.path().join(concat!($name, ".tif"));
            fixture(&path, values, 1);
            let source = open_source_for_compile(&spec(&path), &cancel).unwrap();
            let metadata = source.raw_metadata().unwrap();
            assert_eq!(metadata.bands[0].scalar_type, $kind);
            assert_eq!(metadata.bands[0].original_band_index, 0);
            assert_eq!(metadata.bands[0].description, "logical-band-1");
            assert_eq!(metadata.bands[0].unit.as_deref(), Some("test unit"));
            assert_eq!(metadata.pixel_convention, "Point");
            let gdal = gdal::Dataset::open(&path).unwrap();
            assert_eq!(
                metadata.bands[0].nodata_f64_bits,
                gdal.rasterband(1)
                    .unwrap()
                    .no_data_value()
                    .map(f64::to_bits)
            );
            assert_eq!(
                metadata.bands[0].offset_f64_bits,
                gdal.rasterband(1).unwrap().offset().unwrap_or(0.).to_bits()
            );
            let serialized = serde_json::to_vec(metadata).unwrap();
            let decoded: RawRasterMetadata = serde_json::from_slice(&serialized).unwrap();
            assert_eq!(
                decoded.bands[0].nodata_f64_bits,
                metadata.bands[0].nodata_f64_bits
            );
            let bound = source.raw_read_buffer_bound(4, 2, &[0]).unwrap();
            assert!(
                source
                    .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], bound - 1, &cancel)
                    .is_err()
            );
            let (raw, metrics) = source
                .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], bound, &cancel)
                .unwrap();
            assert_eq!(raw.bands[0].samples_le, expected, $name);
            assert_eq!(raw.bands[0].mask, vec![255, 127, 0, 1, 255, 0, 254, 255]);
            assert_eq!(metrics.raster_io_calls, 2);
            assert_eq!(metrics.normalization_ms, 0.);
            let normalized = raw
                .normalize(
                    metadata,
                    source.metadata(),
                    0,
                    0,
                    &[0],
                    1024 * 1024,
                    &cancel,
                )
                .unwrap();
            let (ordinary, _) = source
                .read_selected_window_cancellable(0, 0, 4, 2, &[0], 1024 * 1024, &cancel)
                .unwrap();
            assert_eq!(normalized.bands[0].valid, ordinary.bands[0].valid);
            assert_eq!(
                normalized.bands[0]
                    .values
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                ordinary.bands[0]
                    .values
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>()
            );
            let (edge, _) = source
                .read_raw_selected_window_cancellable(1, 1, 3, 1, &[0], 1024 * 1024, &cancel)
                .unwrap();
            assert_eq!(edge.bands[0].samples_le, expected[5 * $kind.byte_width()..]);
            for codec in ["none", "deflate"] {
                let output = dir.path().join(format!("{}-{codec}.skv", $name));
                let options = raster_engine::skv::CompileOptions {
                    chunk_edge: 64,
                    codec: codec.into(),
                    ..Default::default()
                };
                raster_engine::skv::compile(
                    source.as_ref(),
                    output.to_str().unwrap(),
                    &options,
                    &cancel,
                )
                .unwrap();
                let serving = open_source(&spec(&output), &cancel).unwrap();
                assert_eq!(
                    serde_json::to_vec(serving.raw_metadata().unwrap()).unwrap(),
                    serialized,
                    "{} {codec}: typed interpretation metadata",
                    $name
                );
                assert_eq!(
                    serde_json::to_value(&serving.metadata().grid).unwrap(),
                    serde_json::to_value(&source.metadata().grid).unwrap()
                );
                let bound = serving.raw_read_buffer_bound(4, 2, &[0]).unwrap();
                let (round_trip, _) = serving
                    .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], bound, &cancel)
                    .unwrap();
                assert_eq!(
                    round_trip.bands[0].samples_le, expected,
                    "{} {codec}",
                    $name
                );
                assert_eq!(round_trip.bands[0].mask, raw.bands[0].mask);
                let (read, _) = serving
                    .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
                    .unwrap();
                assert_eq!(read.bands[0].valid, ordinary.bands[0].valid);
                assert_eq!(
                    read.bands[0]
                        .values
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    ordinary.bands[0]
                        .values
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    "{} {codec}: normalized result bits",
                    $name
                );
                let (read_edge, _) = serving
                    .read_raw_selected_window_cancellable(1, 1, 3, 1, &[0], 16 << 20, &cancel)
                    .unwrap();
                assert_eq!(read_edge.bands[0].samples_le, edge.bands[0].samples_le);
                assert_eq!(read_edge.bands[0].mask, edge.bands[0].mask);
                let verified = raster_engine::skv::verify(&spec(&output), &cancel).unwrap();
                assert_eq!(verified["verified"], true);
                assert_eq!(verified["original_source_opened"], false);
                assert_eq!(verified["decoded_raw_bytes"], 8 * ($kind.byte_width() + 1));
                assert_eq!(verified["summary_states_verified"], 1);
            }
        }};
    }
    check!(
        "byte",
        RawScalarType::Byte,
        vec![0u8, 1, 127, 128, 254, 255, 22, 99]
    );
    check!(
        "int8",
        RawScalarType::Int8,
        vec![0i8, 1, -1, -128, 127, 12, -12, 99]
    );
    check!(
        "uint16",
        RawScalarType::UInt16,
        vec![0u16, 1, 255, 256, 32767, 65535, 99, 1234]
    );
    check!(
        "int16",
        RawScalarType::Int16,
        vec![0i16, 1, -1, i16::MIN, i16::MAX, 123, -123, 99]
    );
    check!(
        "uint32",
        RawScalarType::UInt32,
        vec![0u32, 1, 65535, 65536, u32::MAX, 99, 100, 999]
    );
    check!(
        "int32",
        RawScalarType::Int32,
        vec![0i32, 1, -1, i32::MIN, i32::MAX, 123, -123, 99]
    );
    check!(
        "float32",
        RawScalarType::Float32,
        vec![
            0f32,
            -0.,
            f32::from_bits(0x7fc1_2345),
            f32::from_bits(0x7fa1_2345),
            f32::NEG_INFINITY,
            f32::INFINITY,
            -42.25,
            f32::from_bits(1)
        ]
    );
    check!(
        "float64",
        RawScalarType::Float64,
        vec![
            0f64,
            -0.,
            f64::from_bits(0x7ff8_1234_5678_1234),
            f64::from_bits(0x7ff4_1234_5678_1234),
            f64::NEG_INFINITY,
            f64::INFINITY,
            -42.25,
            f64::from_bits(1)
        ]
    );
}

#[test]
fn forty_band_metadata_is_bounded_while_reads_stay_grouped_and_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forty.tif");
    fixture(&path, vec![0u16, 1, 2, 3, 4, 5, 6, 7], 40);
    {
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )
        .unwrap();
        for index in 1..=40 {
            dataset
                .rasterband(index)
                .unwrap()
                .write(
                    (0, 0),
                    (4, 2),
                    &mut Buffer::new((4, 2), vec![index as u16; 8]),
                )
                .unwrap();
        }
    }
    let cancel = AtomicBool::new(false);
    let source = open_source(&spec(&path), &cancel).unwrap();
    assert_eq!(source.metadata().bands.len(), 40);
    assert_eq!(source.raw_metadata().unwrap().source_band_count, 40);
    let all = (0..40).collect::<Vec<_>>();
    assert!(source.raw_read_buffer_bound(4, 2, &all).is_err());
    assert!(source.read_buffer_bound(4, 2, &all).is_err());
    for group in all.chunks(20) {
        let (raw, _) = source
            .read_raw_selected_window_cancellable(0, 0, 4, 2, group, 8 * 1024 * 1024, &cancel)
            .unwrap();
        for (&index, band) in group.iter().zip(raw.bands) {
            assert_eq!(
                band.samples_le,
                (0..8)
                    .flat_map(|_| ((index + 1) as u16).to_le_bytes())
                    .collect::<Vec<_>>()
            );
        }
    }
    let mut mapped_spec = spec(&path);
    mapped_spec.bands = Some(vec![39, 0, 21]);
    let mapped = open_source_for_compile(&mapped_spec, &cancel).unwrap();
    let rawmeta = mapped.raw_metadata().unwrap();
    assert_eq!(
        rawmeta
            .bands
            .iter()
            .map(|b| b.original_band_index)
            .collect::<Vec<_>>(),
        vec![39, 0, 21]
    );
    let (raw, _) = mapped
        .read_raw_selected_window_cancellable(0, 0, 4, 2, &[2, 0], 8 * 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(
        u16::from_le_bytes(raw.bands[0].samples_le[..2].try_into().unwrap()),
        22
    );
    assert_eq!(
        u16::from_le_bytes(raw.bands[1].samples_le[..2].try_into().unwrap()),
        40
    );
}

#[test]
fn raw_failure_paths_cancel_mutation_and_malformed_normalization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("typed.tif");
    fixture(&path, vec![0i32, 1, 2, 3, 4, 5, 6, 7], 1);
    let cancel = AtomicBool::new(false);
    let source = open_source_for_compile(&spec(&path), &cancel).unwrap();
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        source
            .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], 1024 * 1024, &cancelled)
            .err()
            .unwrap()
            .to_string(),
        "cancelled"
    );
    assert!(
        source
            .read_raw_selected_window_cancellable(usize::MAX, 0, 4, 2, &[0], 1024 * 1024, &cancel)
            .is_err()
    );
    assert!(source.raw_read_buffer_bound(0, 2, &[0]).is_err());
    assert!(source.raw_read_buffer_bound(4, 2, &[0, 0]).is_err());
    let (mut raw, _) = source
        .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], 1024 * 1024, &cancel)
        .unwrap();
    raw.bands[0].mask.pop();
    assert!(
        raw.normalize(
            source.raw_metadata().unwrap(),
            source.metadata(),
            0,
            0,
            &[0],
            1024 * 1024,
            &cancel
        )
        .is_err()
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(
        source
            .read_raw_selected_window_cancellable(0, 0, 4, 2, &[0], 1024 * 1024, &cancel)
            .err()
            .unwrap()
            .to_string()
            .contains("source changed")
    );
}

#[test]
fn raw_nodata_nan_is_retained_as_metadata_bits_and_unsupported_types_reject() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nan-nodata.tif");
    fixture(&path, vec![0f64, 1., 2., 3., 4., 5., 6., 7.], 1);
    {
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )
        .unwrap();
        dataset
            .rasterband(1)
            .unwrap()
            .set_no_data_value(Some(f64::from_bits(0x7ff8_1234_1234_1234)))
            .unwrap();
    }
    let cancel = AtomicBool::new(false);
    let source = open_source_for_compile(&spec(&path), &cancel).unwrap();
    let metadata = source.raw_metadata().unwrap();
    let bits = metadata.bands[0].nodata_f64_bits.unwrap();
    assert!(f64::from_bits(bits).is_nan());
    let from_json: RawRasterMetadata =
        serde_json::from_slice(&serde_json::to_vec(metadata).unwrap()).unwrap();
    assert_eq!(from_json.bands[0].nodata_f64_bits, Some(bits));
    let unsupported = dir.path().join("int64.tif");
    fixture(&unsupported, vec![i64::MAX; 8], 1);
    assert!(open_source_for_compile(&spec(&unsupported), &cancel).is_err());
    assert!(RawScalarType::from_gdal_name("UInt64").is_err());
    assert!(RawScalarType::from_gdal_name("CFloat32").is_err());
}

#[test]
fn big_endian_bigtiff_preserves_float_payload_bits_in_little_endian_raw_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big-endian.tif");
    let values = vec![
        f32::from_bits(0x7fc1_2345),
        -0f32,
        f32::from_bits(1),
        -10.25,
    ];
    let options = gdal::raster::RasterCreationOptions::from_iter(["ENDIANNESS=BIG", "BIGTIFF=YES"]);
    {
        let mut dataset = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type_with_options::<f32, _>(&path, 2, 2, 1, &options)
            .unwrap();
        dataset
            .set_geo_transform(&[0., 1., 0., 2., 0., -1.])
            .unwrap();
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        dataset
            .rasterband(1)
            .unwrap()
            .write((0, 0), (2, 2), &mut Buffer::new((2, 2), values.clone()))
            .unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..4], b"MM\0+");
    let cancel = AtomicBool::new(false);
    let source = open_source_for_compile(&spec(&path), &cancel).unwrap();
    assert_eq!(source.raw_metadata().unwrap().bands[0].mask_flags, 1);
    let (raw, _) = source
        .read_raw_selected_window_cancellable(0, 0, 2, 2, &[0], 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(
        raw.bands[0].samples_le,
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    );
    assert_eq!(raw.bands[0].mask, vec![255; 4]);
}
