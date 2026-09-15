use anyhow::{Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use raster_engine::{
    aggregate::Options,
    batch::{BorrowedSource, Job, JobSpec},
    io::open_source,
    model::{Grid, Raster, check_cancel},
    skv::{self, BOOTSTRAP, CompileOptions, PAGE, RECORD, RECORDS_PER_PAGE, SkvSource},
    source::{
        BandMetadata, RasterMetadata, RawBandMetadata, RawBandWindow, RawRasterMetadata,
        RawScalarType, RawWindow, ReadMetrics, SourceSpec, WindowSource,
    },
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

struct TypedFixture {
    metadata: RasterMetadata,
    raw_metadata: RawRasterMetadata,
    data: Vec<RawBandWindow>,
}
impl TypedFixture {
    fn new(width: usize, height: usize, kinds: &[RawScalarType], finite: bool) -> Self {
        let raw_metadata = RawRasterMetadata {
            bands: kinds
                .iter()
                .enumerate()
                .map(|(b, &kind)| RawBandMetadata {
                    scalar_type: kind,
                    nodata_f64_bits: Some((-0.0f64).to_bits()),
                    scale_f64_bits: 2.0f64.to_bits(),
                    offset_f64_bits: (-0.0f64).to_bits(),
                    unit: Some("test unit".into()),
                    mask_flags: 4,
                    original_band_index: b,
                    description: format!("typed-{b}"),
                })
                .collect(),
            pixel_convention: "Point".into(),
            source_band_count: kinds.len(),
            source_overview: None,
        };
        let metadata = RasterMetadata {
            grid: Grid {
                width,
                height,
                transform: [0., 1., 0., height as f64, 0., -1.],
                crs: "LOCAL".into(),
            },
            bands: raw_metadata
                .bands
                .iter()
                .map(|b| BandMetadata {
                    data_type: format!("{:?}", b.scalar_type),
                    nodata: Some(-0.),
                    scale: 2.,
                    offset: -0.,
                    unit: b.unit.clone(),
                    block_size: (256, 256),
                })
                .collect(),
            source_id: "independent-typed-predictor-fixture".into(),
        };
        let data = kinds
            .iter()
            .enumerate()
            .map(|(b, &kind)| {
                let scalar = kind.byte_width();
                let mut samples_le = Vec::with_capacity(width * height * scalar);
                for cell in 0..width * height {
                    let bits = if finite {
                        assert_eq!(kind, RawScalarType::Float64);
                        (1000. * (b + 1) as f64 + cell as f64 * 0.25).to_bits()
                    } else {
                        let i = cell % 10;
                        match kind {
                            RawScalarType::Float32 => [
                                0, 0x80000000, 0x3f800000, 0xc0000000, 1, 0x7fc01234, 0x7f801234,
                                0x7f800000, 0xff800000, 0x42288000,
                            ][i],
                            RawScalarType::Float64 => [
                                0,
                                0x8000000000000000,
                                0x3ff0000000000000,
                                0xc000000000000000,
                                1,
                                0x7ff8000012345678,
                                0x7ff0000012345678,
                                0x7ff0000000000000,
                                0xfff0000000000000,
                                0x4045100000000000,
                            ][i],
                            _ => [
                                0,
                                1,
                                u64::MAX,
                                0x80808080,
                                0x7fffffff,
                                0xff00ff00,
                                0x12345678,
                                0x100,
                                0xffff,
                                42,
                            ][i],
                        }
                    };
                    samples_le.extend_from_slice(&bits.to_le_bytes()[..scalar]);
                }
                let mask = (0..width * height)
                    .map(|i| [255, 127, 0, 1, 254][(i + b) % 5])
                    .collect();
                RawBandWindow { samples_le, mask }
            })
            .collect();
        Self {
            metadata,
            raw_metadata,
            data,
        }
    }
}
impl WindowSource for TypedFixture {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        Some(&self.raw_metadata)
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        Ok(w * h
            * bands
                .iter()
                .map(|&b| self.raw_metadata.bands[b].scalar_type.byte_width() + 1)
                .sum::<usize>())
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
            x + w <= self.metadata.grid.width
                && y + h <= self.metadata.grid.height
                && bands.len() <= 20,
            "fixture window"
        );
        ensure!(
            self.raw_read_buffer_bound(w, h, bands)? <= max,
            "fixture budget"
        );
        let data = bands
            .iter()
            .map(|&b| {
                let scalar = self.raw_metadata.bands[b].scalar_type.byte_width();
                let mut samples_le = Vec::with_capacity(w * h * scalar);
                let mut mask = Vec::with_capacity(w * h);
                for row in y..y + h {
                    let first = row * self.metadata.grid.width + x;
                    samples_le.extend_from_slice(
                        &self.data[b].samples_le[first * scalar..(first + w) * scalar],
                    );
                    mask.extend_from_slice(&self.data[b].mask[first..first + w]);
                }
                RawBandWindow { samples_le, mask }
            })
            .collect();
        Ok((
            RawWindow {
                width: w,
                height: h,
                bands: data,
            },
            ReadMetrics {
                raster_io_calls: 1,
                ..Default::default()
            },
        ))
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
        let (raw, metrics) =
            self.read_raw_selected_window_cancellable(x, y, w, h, bands, max, cancel)?;
        Ok((
            raw.normalize(&self.raw_metadata, &self.metadata, x, y, bands, max, cancel)?,
            metrics,
        ))
    }
}
fn spec(path: &Path) -> SourceSpec {
    serde_json::from_value(json!({"location":path})).unwrap()
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn header(bytes: &[u8]) -> Value {
    let mut decoded = Vec::new();
    ZlibDecoder::new(&bytes[64..64 + u32_at(bytes, 24) as usize])
        .read_to_end(&mut decoded)
        .unwrap();
    serde_json::from_slice(&decoded).unwrap()
}
fn rewrite_header(bytes: &mut [u8], metadata: &Value, flags: u32) {
    let decoded = serde_json::to_vec(metadata).unwrap();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(3));
    encoder.write_all(&decoded).unwrap();
    let encoded = encoder.finish().unwrap();
    bytes[64..BOOTSTRAP - 32].fill(0);
    bytes[64..64 + encoded.len()].copy_from_slice(&encoded);
    bytes[12..16].copy_from_slice(&flags.to_le_bytes());
    bytes[24..28].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
    bytes[28..32].copy_from_slice(&(decoded.len() as u32).to_le_bytes());
    let digest = blake3::hash(&bytes[..BOOTSTRAP - 32]);
    bytes[BOOTSTRAP - 32..BOOTSTRAP].copy_from_slice(digest.as_bytes());
}
fn record_offset(id: usize) -> usize {
    BOOTSTRAP + (id / RECORDS_PER_PAGE) * PAGE + 16 + (id % RECORDS_PER_PAGE) * RECORD
}
fn decoded_encoded_payload(bytes: &[u8], id: usize) -> Vec<u8> {
    let at = record_offset(id);
    let offset = u64_at(bytes, at) as usize;
    let payload = &bytes[offset..offset + u32_at(bytes, at + 8) as usize];
    if u32_at(bytes, at + 24) == 0 {
        payload.to_vec()
    } else {
        let mut decoded = Vec::new();
        ZlibDecoder::new(payload).read_to_end(&mut decoded).unwrap();
        decoded
    }
}
// Independent specification encoder: first gather one plane-row into a Vec,
// then take adjacent wrapping differences. No production helper is invoked.
fn expected_payload(raw: &RawBandWindow, w: usize, h: usize, scalar: usize) -> Vec<u8> {
    let mut result = Vec::new();
    for plane in 0..scalar {
        for row in 0..h {
            let line: Vec<u8> = (0..w)
                .map(|x| raw.samples_le[(row * w + x) * scalar + plane])
                .collect();
            result.push(line[0]);
            result.extend(line.windows(2).map(|pair| pair[1].wrapping_sub(pair[0])));
        }
    }
    result.extend_from_slice(&raw.mask);
    result
}
fn assert_normalized_equal(a: &Raster, b: &Raster) {
    for (a, b) in a.bands.iter().zip(&b.bands) {
        assert_eq!(a.valid, b.valid);
        assert_eq!(
            a.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            b.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn grouped_rows_preserve_mixed_types_masks_payload_bits_and_old_logical_identity() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let kinds = [
        RawScalarType::Byte,
        RawScalarType::Int8,
        RawScalarType::UInt16,
        RawScalarType::Int16,
        RawScalarType::UInt32,
        RawScalarType::Int32,
        RawScalarType::Float32,
        RawScalarType::Float64,
    ];
    for width in [1, 257] {
        let source = TypedFixture::new(width, 3, &kinds, false);
        let bands = (0..8).collect::<Vec<_>>();
        let (ordinary, _) = source
            .read_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
            .unwrap();
        let baseline = dir.path().join(format!("baseline-{width}.skv"));
        let control = skv::compile(
            &source,
            baseline.to_str().unwrap(),
            &CompileOptions::default(),
            &cancel,
        )
        .unwrap();
        for codec in ["none", "deflate"] {
            for predictor in ["none", "byte_delta_v1"] {
                let path = dir
                    .path()
                    .join(format!("group-{width}-{codec}-{predictor}.skv"));
                let receipt = skv::compile(
                    &source,
                    path.to_str().unwrap(),
                    &CompileOptions {
                        payload_layout: "row_group_v1".into(),
                        band_group: 64,
                        codec: codec.into(),
                        predictor: predictor.into(),
                        ..Default::default()
                    },
                    &cancel,
                )
                .unwrap();
                assert_eq!(receipt["payload_layout"], "row_group_v1");
                assert_eq!(receipt["logical_digest"], control["logical_digest"]);
                assert_eq!(receipt["maximum_group_bytes"], 3_276_800);
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(u32_at(&bytes, 12) & 8, 8);
                assert_eq!(header(&bytes)["payload_layout"], "row_group_v1");
                for (node, x0) in (0..width).step_by(256).enumerate() {
                    let w = (width - x0).min(256);
                    let (raw, _) = source
                        .read_raw_selected_window_cancellable(
                            x0,
                            0,
                            w,
                            3,
                            &bands,
                            16 << 20,
                            &cancel,
                        )
                        .unwrap();
                    for (first, count) in [(0, 2), (2, 2), (4, 3), (7, 1)] {
                        let scalar = kinds[first].byte_width();
                        let mut expected = Vec::new();
                        for row in 0..3 {
                            for plane in 0..scalar {
                                for x in 0..w {
                                    for b in first..first + count {
                                        let current =
                                            raw.bands[b].samples_le[(row * w + x) * scalar + plane];
                                        let previous = if x == 0 {
                                            0
                                        } else {
                                            raw.bands[b].samples_le
                                                [(row * w + x - 1) * scalar + plane]
                                        };
                                        expected.push(if predictor == "none" {
                                            current
                                        } else {
                                            current.wrapping_sub(previous)
                                        });
                                    }
                                }
                            }
                        }
                        for row in 0..3 {
                            for x in 0..w {
                                for b in first..first + count {
                                    expected.push(raw.bands[b].mask[row * w + x]);
                                }
                            }
                        }
                        let leader = node * 8 + first;
                        assert_eq!(
                            decoded_encoded_payload(&bytes, leader),
                            expected,
                            "independent grouped byte ordering"
                        );
                        for b in first..first + count {
                            let at = record_offset(node * 8 + b);
                            assert_eq!(u64_at(&bytes, at + 112), leader as u64);
                            assert_eq!(u32_at(&bytes, at + 120), count as u32);
                        }
                    }
                }
                let mut mapped = spec(&path);
                mapped.bands = Some(vec![7, 0, 6, 3]);
                let serving = SkvSource::open(&spec(&path), &cancel).unwrap();
                let (roundtrip, _) = serving
                    .read_raw_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
                    .unwrap();
                for b in 0..8 {
                    assert_eq!(roundtrip.bands[b].samples_le, source.data[b].samples_le);
                    assert_eq!(roundtrip.bands[b].mask, source.data[b].mask);
                }
                let (normal, _) = serving
                    .read_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
                    .unwrap();
                assert_normalized_equal(&ordinary, &normal);
                let view = SkvSource::open(&mapped, &cancel).unwrap();
                let (last, _) = view
                    .read_raw_selected_window_cancellable(
                        width - 1,
                        2,
                        1,
                        1,
                        &[3, 0, 2, 1],
                        16 << 20,
                        &cancel,
                    )
                    .unwrap();
                for (out, b) in [3, 7, 6, 0].into_iter().enumerate() {
                    let scalar = kinds[b].byte_width();
                    assert_eq!(
                        last.bands[out].samples_le,
                        source.data[b].samples_le[(width * 3 - 1) * scalar..]
                    );
                }
                let verified = skv::verify(&spec(&path), &cancel).unwrap();
                let total = source
                    .data
                    .iter()
                    .map(|b| b.samples_le.len() + b.mask.len())
                    .sum::<usize>();
                assert_eq!(verified["decoded_raw_bytes"], total);
                assert_eq!(
                    verified["diagnostics"]["metrics"]["raw_decoded_bytes"], total,
                    "full verification decodes each physical group once"
                );
                assert_eq!(verified["logical_digest"], control["logical_digest"]);
                let mut wrong = bytes.clone();
                let mut h = header(&wrong);
                h.as_object_mut().unwrap().remove("payload_layout");
                rewrite_header(&mut wrong, &h, u32_at(&bytes, 12));
                let invalid = dir
                    .path()
                    .join(format!("wrong-{width}-{codec}-{predictor}.skv"));
                std::fs::write(&invalid, wrong).unwrap();
                assert!(
                    SkvSource::open(&spec(&invalid), &cancel).is_err(),
                    "flag cannot silently fall back to old layout"
                );
            }
        }
    }
}

#[test]
fn grouped_full_tile_forty_and_sixtyfour_respect_packet_and_read_caps() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    for (bands, kind, finite, groups) in [
        (40, RawScalarType::Float32, false, 1),
        (64, RawScalarType::Float64, true, 3),
    ] {
        let source = TypedFixture::new(128, 128, &vec![kind; bands], finite);
        let path = dir.path().join(format!("wide-{bands}.skv"));
        let receipt = skv::compile(
            &source,
            path.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: 128,
                band_group: 64,
                payload_layout: "row_group_v1".into(),
                predictor: "byte_delta_v1".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(receipt["physical_payloads"], groups);
        let selected = (0..bands).rev().collect::<Vec<_>>();
        let serving = SkvSource::open(&spec(&path), &cancel).unwrap();
        let bound = serving.raw_read_buffer_bound(128, 128, &selected).unwrap();
        assert_eq!(
            bound,
            8 * 1024 * 1024 + 128 * 128 * bands * (kind.byte_width() + 1)
        );
        let (raw, _) = serving
            .read_raw_selected_window_cancellable(0, 0, 128, 128, &selected, bound, &cancel)
            .unwrap();
        for (out, b) in selected.iter().enumerate() {
            assert_eq!(raw.bands[out].samples_le, source.data[*b].samples_le);
            assert_eq!(raw.bands[out].mask, source.data[*b].mask);
        }
        assert_eq!(
            serving.diagnostics()["metrics"]["raw_decoded_bytes"],
            128 * 128 * bands * (kind.byte_width() + 1)
        );
        let (_, normal_metrics) = serving
            .read_selected_window_cancellable(0, 0, 128, 128, &selected, 64 << 20, &cancel)
            .unwrap();
        assert!(normal_metrics.raster_io_calls > 0);
        let limited = SkvSource::open(&spec(&path), &cancel).unwrap();
        assert!(
            limited
                .read_raw_selected_window_cancellable(0, 0, 128, 128, &selected, bound - 1, &cancel)
                .is_err()
        );
        assert_eq!(limited.diagnostics()["metrics"]["raw_encoded_bytes"], 0);
    }
}

#[test]
fn predictor_preserves_all_types_bits_masks_edges_and_logical_digest() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let kinds = [
        RawScalarType::Byte,
        RawScalarType::Int8,
        RawScalarType::UInt16,
        RawScalarType::Int16,
        RawScalarType::UInt32,
        RawScalarType::Int32,
        RawScalarType::Float32,
        RawScalarType::Float64,
    ];
    for width in [1, 255, 256, 257] {
        let source = TypedFixture::new(width, 3, &kinds, false);
        let bands: Vec<usize> = (0..8).collect();
        let (ordinary, _) = source
            .read_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
            .unwrap();
        let mut logical = None;
        for codec in ["none", "deflate"] {
            for predictor in ["none", "byte_delta_v1"] {
                let path = dir.path().join(format!("{width}-{codec}-{predictor}.skv"));
                let receipt = skv::compile(
                    &source,
                    path.to_str().unwrap(),
                    &CompileOptions {
                        codec: codec.into(),
                        predictor: predictor.into(),
                        ..Default::default()
                    },
                    &cancel,
                )
                .unwrap();
                assert_eq!(receipt["predictor"], predictor);
                if let Some(expected) = &logical {
                    assert_eq!(&receipt["logical_digest"], expected);
                } else {
                    logical = Some(receipt["logical_digest"].clone());
                }
                assert_eq!(
                    receipt["predictor_scratch_peak_bytes"],
                    if predictor == "none" {
                        0
                    } else {
                        width.min(256) * 3 * 8
                    }
                );
                let bytes = std::fs::read(&path).unwrap();
                let metadata = header(&bytes);
                assert_eq!(
                    metadata.get("predictor").and_then(Value::as_str),
                    if predictor == "none" {
                        None
                    } else {
                        Some(predictor)
                    }
                );
                assert_eq!(
                    u32_at(&bytes, 12) & 2,
                    if predictor == "none" { 0 } else { 2 }
                );
                for (node, x) in (0..width).step_by(256).enumerate() {
                    let w = (width - x).min(256);
                    let (raw, _) = source
                        .read_raw_selected_window_cancellable(x, 0, w, 3, &bands, 16 << 20, &cancel)
                        .unwrap();
                    for (b, &kind) in kinds.iter().enumerate() {
                        let expected = if predictor == "none" {
                            [
                                raw.bands[b].samples_le.as_slice(),
                                raw.bands[b].mask.as_slice(),
                            ]
                            .concat()
                        } else {
                            expected_payload(&raw.bands[b], w, 3, kind.byte_width())
                        };
                        assert_eq!(
                            decoded_encoded_payload(&bytes, node * 8 + b),
                            expected,
                            "stored byte-plane ordering width {width} band {b}"
                        );
                    }
                }
                let serving = SkvSource::open(&spec(&path), &cancel).unwrap();
                let (roundtrip, _) = serving
                    .read_raw_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
                    .unwrap();
                for b in 0..8 {
                    assert_eq!(roundtrip.bands[b].samples_le, source.data[b].samples_le);
                    assert_eq!(roundtrip.bands[b].mask, source.data[b].mask);
                }
                let (normalized, _) = serving
                    .read_selected_window_cancellable(0, 0, width, 3, &bands, 16 << 20, &cancel)
                    .unwrap();
                assert_normalized_equal(&ordinary, &normalized);
                let mut mapped_spec = spec(&path);
                mapped_spec.bands = Some(vec![7, 0, 6, 3]);
                let mapped = SkvSource::open(&mapped_spec, &cancel).unwrap();
                let (selected, _) = mapped
                    .read_raw_selected_window_cancellable(
                        width - 1,
                        2,
                        1,
                        1,
                        &[0, 1, 2, 3],
                        16 << 20,
                        &cancel,
                    )
                    .unwrap();
                for (out, b) in [7, 0, 6, 3].into_iter().enumerate() {
                    let scalar = kinds[b].byte_width();
                    assert_eq!(
                        selected.bands[out].samples_le,
                        source.data[b].samples_le[(width * 3 - 1) * scalar..]
                    );
                }
                let verified = skv::verify(&spec(&path), &cancel).unwrap();
                assert_eq!(verified["verified"], true);
                let metrics = serving.diagnostics()["metrics"].clone();
                assert_eq!(
                    metrics["predictor_scratch_peak_bytes"],
                    receipt["predictor_scratch_peak_bytes"]
                );
                assert_eq!(
                    metrics["predictor_decode_ms"].as_f64().unwrap() > 0.,
                    predictor != "none"
                );
                assert_eq!(
                    receipt["predictor_encode_ms"].as_f64().unwrap() > 0.,
                    predictor != "none"
                );
            }
        }
    }
}

#[test]
fn full_tile_scratch_remains_bounded_and_cancelled_work_is_not_published() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let source = TypedFixture::new(256, 256, &[RawScalarType::Float64], false);
    let path = dir.path().join("largest.skv");
    let options = CompileOptions {
        predictor: "byte_delta_v1".into(),
        ..Default::default()
    };
    let receipt = skv::compile(&source, path.to_str().unwrap(), &options, &cancel).unwrap();
    assert_eq!(receipt["predictor_scratch_peak_bytes"], 524288);
    let serving = SkvSource::open(&spec(&path), &cancel).unwrap();
    assert_eq!(
        serving.raw_read_buffer_bound(256, 256, &[0]).unwrap(),
        (8 << 20) + 256 * 256 * 9
    );
    let (raw, _) = serving
        .read_raw_selected_window_cancellable(0, 0, 256, 256, &[0], 16 << 20, &cancel)
        .unwrap();
    assert_eq!(raw.bands[0].samples_le, source.data[0].samples_le);
    assert_eq!(
        serving.diagnostics()["metrics"]["predictor_scratch_peak_bytes"],
        524288
    );
    let cancelled = AtomicBool::new(true);
    let unpublished = dir.path().join("cancelled.skv");
    assert!(skv::compile(&source, unpublished.to_str().unwrap(), &options, &cancelled).is_err());
    assert!(!unpublished.exists());
    assert!(
        serving
            .read_raw_selected_window_cancellable(0, 0, 256, 256, &[0], 16 << 20, &cancelled)
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn old_missing_field_and_predictor_flags_fail_closed_on_mismatch_or_corruption() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let source = TypedFixture::new(1, 1, &[RawScalarType::Float64], true);
    let old = dir.path().join("old.skv");
    skv::compile(&source, old.to_str().unwrap(), &Default::default(), &cancel).unwrap();
    let oldbytes = std::fs::read(&old).unwrap();
    assert!(header(&oldbytes).get("predictor").is_none());
    assert_eq!(skv::verify(&spec(&old), &cancel).unwrap()["verified"], true);
    let path = dir.path().join("predicted.skv");
    skv::compile(
        &source,
        path.to_str().unwrap(),
        &CompileOptions {
            predictor: "byte_delta_v1".into(),
            codec: "none".into(),
            ..Default::default()
        },
        &cancel,
    )
    .unwrap();
    let original = std::fs::read(&path).unwrap();
    for (name, metadata, flags) in [
        ("missing-required-bit", header(&original), 1),
        ("spurious-required-bit", header(&oldbytes), 3),
        ("unknown-required-bit", header(&original), 7),
        (
            "unknown-predictor",
            {
                let mut h = header(&original);
                h["predictor"] = json!("invented");
                h
            },
            3,
        ),
    ] {
        let mut bytes = original.clone();
        rewrite_header(&mut bytes, &metadata, flags);
        let bad = dir.path().join(format!("{name}.skv"));
        std::fs::write(&bad, bytes).unwrap();
        assert!(SkvSource::open(&spec(&bad), &cancel).is_err(), "{name}");
    }
    let unknown = dir.path().join("unknown.skv");
    let err = skv::compile(
        &source,
        unknown.to_str().unwrap(),
        &CompileOptions {
            predictor: "invented".into(),
            ..Default::default()
        },
        &cancel,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unsupported SKV predictor"));
    assert!(!unknown.exists());
    let at = record_offset(0);
    let offset = u64_at(&original, at) as usize;
    let mut corrupt = original.clone();
    corrupt[offset] ^= 1;
    let bad = dir.path().join("checksum.skv");
    std::fs::write(&bad, &corrupt).unwrap();
    let s = SkvSource::open(&spec(&bad), &cancel).unwrap();
    assert!(
        s.read_raw_selected_window_cancellable(0, 0, 1, 1, &[0], 16 << 20, &cancel)
            .err()
            .expect("must reject corrupt predictor payload")
            .to_string()
            .contains("checksum")
    );
    // Re-sign the independently checksummed payload, directory and header, but
    // retain the logical digest: full verification must see restored-value drift.
    let mut hash = blake3::Hasher::new();
    hash.update(&0u64.to_le_bytes());
    hash.update(&corrupt[offset..]);
    corrupt[at + 32..at + 64].copy_from_slice(hash.finalize().as_bytes());
    let digest = blake3::hash(&corrupt[BOOTSTRAP..BOOTSTRAP + PAGE - 32]);
    corrupt[BOOTSTRAP + PAGE - 32..BOOTSTRAP + PAGE].copy_from_slice(digest.as_bytes());
    let mut metadata = header(&corrupt);
    metadata["directory_digest"] = json!(
        blake3::hash(&corrupt[BOOTSTRAP..BOOTSTRAP + PAGE])
            .to_hex()
            .to_string()
    );
    rewrite_header(&mut corrupt, &metadata, 3);
    let bad = dir.path().join("coherent-wrong-values.skv");
    std::fs::write(&bad, corrupt).unwrap();
    assert!(skv::verify(&spec(&bad), &cancel).is_err());
    let mut short = original.clone();
    short.truncate(short.len() - 1);
    let bad = dir.path().join("short.skv");
    std::fs::write(&bad, short).unwrap();
    assert!(SkvSource::open(&spec(&bad), &cancel).is_err());
}

fn polygon(left: f64, right: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[left,0.25],[right,0.25],[right,2.75],[left,2.75],[left,0.25]]]})
}
fn options() -> Options {
    Options {
        statistics: Some(
            ["sum", "support", "mean", "min", "max", "count"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Default::default()
    }
}
fn single(source: &dyn WindowSource, geometry: &Value) -> Value {
    streaming::measure_cached_source(
        source,
        geometry,
        "LOCAL",
        &options(),
        &AtomicBool::new(false),
        &mut TileCache::default(),
        0.,
    )
    .unwrap()
}
fn batch(source: &dyn WindowSource, zones: &[Value]) -> Value {
    let spec:JobSpec=serde_json::from_value(json!({"zones":zones.iter().enumerate().map(|(i,g)|json!({"id":i.to_string(),"version":"1","geometry":g})).collect::<Vec<_>>(),"slices":[{"id":"one","source":"r"}],"crs":"LOCAL","tile_edge":64,"options":options()})).unwrap();
    Job::new(spec, None, 4096, 1 << 30)
        .unwrap()
        .next(
            zones.len(),
            |_| Ok(Box::new(BorrowedSource(source))),
            &AtomicBool::new(false),
        )
        .unwrap()
}
#[test]
fn predicted_sources_preserve_native_single_shared_batch_and_thin_results_for_36_and_40_bands() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    for count in [36, 40] {
        let source = TypedFixture::new(257, 3, &vec![RawScalarType::Float64; count], true);
        let path = dir.path().join(format!("{count}.skv"));
        skv::compile(
            &source,
            path.to_str().unwrap(),
            &CompileOptions {
                predictor: "byte_delta_v1".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let serving = open_source(&spec(&path), &cancel).unwrap();
        let zones = [polygon(0.125, 256.5), polygon(255.25, 255.250000001)];
        for zone in &zones {
            assert_eq!(
                single(&source, zone)["bands"],
                single(serving.as_ref(), zone)["bands"]
            );
        }
        let a = batch(&source, &zones);
        let b = batch(serving.as_ref(), &zones);
        assert_eq!(b["complete"], true);
        assert_eq!(b["metrics"]["geometry_compilations"], 2);
        for i in 0..zones.len() {
            assert_eq!(a["rows"][i]["bands"], b["rows"][i]["bands"]);
        }
        assert!(
            b["rows"][1]["bands"][0]["covered_cell_equivalents"]
                .as_f64()
                .unwrap()
                > 0.
        );
        #[cfg(feature = "exactextract")]
        {
            use raster_engine::{
                backend::EeOptions,
                exactextract::{self, Input},
            };
            let mut opts = options();
            opts.statistics.as_mut().unwrap().retain(|s| s != "count");
            for strategy in ["feature-sequential", "raster-sequential"] {
                let run = |s: &dyn WindowSource| {
                    exactextract::execute(
                        &[Input {
                            source: s,
                            bands: (0..count).collect(),
                        }],
                        &zones,
                        "LOCAL",
                        &opts,
                        &EeOptions {
                            strategy: strategy.into(),
                            ..Default::default()
                        },
                        &cancel,
                        &mut TileCache::default(),
                        1 << 30,
                    )
                    .unwrap()
                };
                let a = run(&source);
                let b = run(serving.as_ref());
                for i in 0..zones.len() {
                    assert_eq!(
                        a.bands(i, 0, opts.statistics.as_ref().unwrap()),
                        b.bands(i, 0, opts.statistics.as_ref().unwrap())
                    );
                }
            }
        }
    }
}

#[test]
fn grouped_fused_cropped_windows_match_original_bits_and_unfused_full_verification() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let kinds = [
        vec![
            RawScalarType::Byte,
            RawScalarType::Int8,
            RawScalarType::UInt16,
            RawScalarType::Int16,
            RawScalarType::UInt32,
            RawScalarType::Int32,
            RawScalarType::Float32,
            RawScalarType::Float64,
        ],
        vec![RawScalarType::Float32; 40],
        vec![RawScalarType::Float64; 64],
    ];
    for (case, kinds) in kinds.iter().enumerate() {
        let source = TypedFixture::new(137, 67, kinds, false);
        for predictor in ["none", "byte_delta_v1"] {
            for codec in ["none", "deflate"] {
                let path = dir
                    .path()
                    .join(format!("fused-{case}-{predictor}-{codec}.skv"));
                let built = skv::compile(
                    &source,
                    path.to_str().unwrap(),
                    &CompileOptions {
                        chunk_edge: 64,
                        band_group: 64,
                        payload_layout: "row_group_v1".into(),
                        predictor: predictor.into(),
                        codec: codec.into(),
                        ..Default::default()
                    },
                    &cancel,
                )
                .unwrap();
                let serving = open_source(&spec(&path), &cancel).unwrap();
                let selected = (0..kinds.len()).rev().collect::<Vec<_>>();
                // Interior x/y offsets require predictor prefixes; later cases
                // cross spatial tiles and reach the clipped bottom/right edge.
                for [x, y, w, h] in [[17, 4, 13, 12], [61, 63, 73, 4], [136, 66, 1, 1]] {
                    let expected = RawWindow {
                        width: w,
                        height: h,
                        bands: selected
                            .iter()
                            .map(|&b| {
                                let scalar = kinds[b].byte_width();
                                let mut samples_le = Vec::new();
                                let mut mask = Vec::new();
                                for row in y..y + h {
                                    let start = row * 137 + x;
                                    samples_le.extend_from_slice(
                                        &source.data[b].samples_le
                                            [start * scalar..(start + w) * scalar],
                                    );
                                    mask.extend_from_slice(&source.data[b].mask[start..start + w]);
                                }
                                RawBandWindow { samples_le, mask }
                            })
                            .collect(),
                    };
                    let (raw, _) = serving
                        .read_raw_selected_window_cancellable(
                            x,
                            y,
                            w,
                            h,
                            &selected,
                            64 << 20,
                            &cancel,
                        )
                        .unwrap();
                    for (actual, expected) in raw.bands.iter().zip(&expected.bands) {
                        assert_eq!(actual.samples_le, expected.samples_le);
                        assert_eq!(actual.mask, expected.mask);
                    }
                    let expected = expected
                        .normalize(
                            &source.raw_metadata,
                            &source.metadata,
                            x,
                            y,
                            &selected,
                            64 << 20,
                            &cancel,
                        )
                        .unwrap();
                    let (normalized, _) = serving
                        .read_selected_window_cancellable(x, y, w, h, &selected, 64 << 20, &cancel)
                        .unwrap();
                    assert_eq!(normalized.bands.len(), expected.bands.len());
                    assert_eq!(normalized.grid, expected.grid);
                    assert_normalized_equal(&normalized, &expected);
                }
                let metrics = serving.diagnostics()["metrics"].clone();
                assert!(metrics["group_restore_ms"].as_f64().unwrap() > 0.);
                assert_eq!(metrics["group_unpack_ms"], 0.);
                assert_eq!(metrics["predictor_decode_ms"], 0.);
                let verified = skv::verify(&spec(&path), &cancel).unwrap();
                assert_eq!(verified["logical_digest"], built["logical_digest"]);
                assert!(
                    verified["diagnostics"]["metrics"]["group_unpack_ms"]
                        .as_f64()
                        .unwrap()
                        > 0.
                );
                assert_eq!(verified["diagnostics"]["metrics"]["group_restore_ms"], 0.);
                let failed = open_source(&spec(&path), &cancel).unwrap();
                assert!(
                    failed
                        .read_raw_selected_window_cancellable(
                            17,
                            4,
                            13,
                            12,
                            &selected,
                            64 << 20,
                            &AtomicBool::new(true)
                        )
                        .is_err()
                );
                assert!(
                    failed
                        .read_raw_selected_window_cancellable(
                            17,
                            4,
                            13,
                            12,
                            &selected,
                            64 << 20,
                            &cancel
                        )
                        .is_err()
                );
                assert_eq!(failed.diagnostics()["metrics"]["raw_encoded_bytes"], 0);
            }
        }
    }
}

#[test]
fn native_windows_preserve_overflow_masks_signed_zero_and_unusual_band_order() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let mut source = TypedFixture::new(137, 3, &[RawScalarType::Float64; 3], false);
    for (i, (scale, offset, nodata)) in [
        (f64::MAX, -0.0_f64, Some(f64::NAN)),
        (-0.0, -0.0, None),
        (0.125, -17.25, Some(-2.0)),
    ]
    .into_iter()
    .enumerate()
    {
        source.raw_metadata.bands[i].scale_f64_bits = scale.to_bits();
        source.raw_metadata.bands[i].offset_f64_bits = offset.to_bits();
        source.raw_metadata.bands[i].nodata_f64_bits = nodata.map(f64::to_bits);
        source.metadata.bands[i].scale = scale;
        source.metadata.bands[i].offset = offset;
        source.metadata.bands[i].nodata = nodata;
    }
    let selected = [2, 0, 1];
    for layout in ["band", "row_group_v1"] {
        let path = dir.path().join(format!("native-special-{layout}.skv"));
        skv::compile(
            &source,
            path.to_str().unwrap(),
            &CompileOptions {
                payload_layout: layout.into(),
                band_group: 3,
                chunk_edge: 64,
                predictor: "byte_delta_v1".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let serving = open_source(&spec(&path), &cancel).unwrap();
        for [x, y, w, h] in [[17, 0, 23, 3], [61, 1, 76, 2], [136, 2, 1, 1]] {
            let (raw, _) = serving
                .read_raw_selected_window_cancellable(x, y, w, h, &selected, 64 << 20, &cancel)
                .unwrap();
            let expected = raw
                .normalize(
                    serving.raw_metadata().unwrap(),
                    serving.metadata(),
                    x,
                    y,
                    &selected,
                    64 << 20,
                    &cancel,
                )
                .unwrap();
            let bound = serving.read_buffer_bound(w, h, &selected).unwrap();
            let (actual, _) = serving
                .read_selected_window_cancellable(x, y, w, h, &selected, bound, &cancel)
                .unwrap();
            assert_normalized_equal(&expected, &actual);
            assert_eq!(expected.grid, actual.grid);
            assert_eq!(expected.source_id, actual.source_id);
        }
        let metrics = serving.diagnostics()["metrics"].clone();
        assert_eq!(metrics["native_raw_intermediate_allocated_bytes"], 0);
        assert!(metrics["native_normalized_written_bytes"].as_u64().unwrap() > 0);
        assert_eq!(
            metrics["native_row_scratch_peak_bytes"],
            if layout == "band" { 0 } else { 2304 }
        );
        let failed = open_source(&spec(&path), &cancel).unwrap();
        let bound = failed.read_buffer_bound(23, 3, &selected).unwrap();
        assert!(
            failed
                .read_selected_window_cancellable(17, 0, 23, 3, &selected, bound - 1, &cancel)
                .is_err()
        );
        assert_eq!(failed.diagnostics()["metrics"]["raw_encoded_bytes"], 0);
        assert!(
            failed
                .read_selected_window_cancellable(17, 0, 23, 3, &selected, bound, &cancel)
                .is_err()
        );
    }
}

#[test]
fn native_skv_window_rejects_shifted_grid_that_cannot_resolve_a_cell() {
    let cancel = AtomicBool::new(false);
    let dir = tempfile::tempdir().unwrap();
    let mut source = TypedFixture::new(2, 1, &[RawScalarType::Float64], true);
    source.metadata.grid.transform[0] = 9_007_199_254_740_991.;
    source.metadata.grid.validate().unwrap();
    let path = dir.path().join("shifted-grid.skv");
    skv::compile(
        &source,
        path.to_str().unwrap(),
        &CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    let serving = open_source(&spec(&path), &cancel).unwrap();
    assert!(
        serving
            .read_selected_window_cancellable(1, 0, 1, 1, &[0], 64 << 20, &cancel)
            .is_err()
    );
    assert!(serving.verify_immutable().is_err());
}
