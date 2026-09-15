use raster_engine::{bulk::*, re_drop, re_new};
use std::{
    mem::{offset_of, size_of, size_of_val},
    ptr,
};

fn f32_band(values: &[f32], id: u32) -> BulkBand {
    BulkBand {
        data: values.as_ptr().cast(),
        data_bytes: size_of_val(values) as u64,
        byte_offset: 0,
        byte_stride: 4,
        cell_count: values.len() as u64,
        validity: ptr::null(),
        validity_bytes: 0,
        validity_offset: 0,
        nodata: 0.0,
        band_id: id,
        dtype: 1,
        validity_kind: 0,
        flags: 0,
    }
}
fn f64_band(values: &[f64], id: u32) -> BulkBand {
    BulkBand {
        data: values.as_ptr().cast(),
        data_bytes: size_of_val(values) as u64,
        byte_stride: 8,
        cell_count: values.len() as u64,
        dtype: 2,
        ..f32_band(&[], id)
    }
}
fn all() -> BulkSelection {
    BulkSelection {
        data: ptr::null(),
        data_bytes: 0,
        count: 0,
        kind: 0,
        flags: 0,
    }
}
fn selection<T>(values: &[T], kind: u32) -> BulkSelection {
    BulkSelection {
        data: values.as_ptr().cast(),
        data_bytes: size_of_val(values) as u64,
        count: values.len() as u64,
        kind,
        flags: 0,
    }
}
fn window(bands: &[BulkBand], selection: BulkSelection) -> BulkWindow {
    BulkWindow {
        bands: bands.as_ptr(),
        band_count: bands.len() as u32,
        flags: 0,
        selection,
    }
}
fn request(windows: &[BulkWindow], policy: u32) -> BulkRequest {
    BulkRequest {
        abi_version: ABI,
        struct_size: size_of::<BulkRequest>() as u32,
        policy,
        reducers: 15,
        windows: windows.as_ptr(),
        window_count: windows.len() as u64,
        max_payload_bytes: 0,
        max_contributions: 0,
        flags: 0,
    }
}
fn call(req: &BulkRequest) -> (i32, Vec<BulkResult>, BulkMetadata, String) {
    let handle = re_new();
    let sentinel = BulkResult {
        band_id: 918,
        sum: 725.0,
        ..Default::default()
    };
    let mut result = vec![sentinel; 64];
    let mut metadata = BulkMetadata {
        payload_bytes: 927,
        ..Default::default()
    };
    let mut error = [0_u8; 256];
    let code = unsafe {
        re_bulk(
            handle,
            req,
            result.as_mut_ptr(),
            64,
            &mut metadata,
            error.as_mut_ptr(),
            error.len() as u64,
        )
    };
    unsafe {
        re_drop(handle);
    }
    let error =
        String::from_utf8_lossy(&error[..error.iter().position(|b| *b == 0).unwrap()]).into_owned();
    if code != 0 {
        assert_eq!(result, vec![sentinel; 64], "error exposed a partial result");
        assert_eq!(metadata.payload_bytes, 927, "error mutated metadata");
    }
    (code, result, metadata, error)
}
fn rejection(req: &BulkRequest, expected: &str) {
    let (code, _, _, error) = call(req);
    assert_eq!(code, INVALID, "{error}");
    assert!(error.contains(expected), "{error}");
}
#[test]
fn native_layout_matches_versioned_header() {
    assert_eq!(size_of::<BulkBand>(), 88);
    assert_eq!(size_of::<BulkSelection>(), 32);
    assert_eq!(size_of::<BulkWindow>(), 48);
    assert_eq!(size_of::<BulkRequest>(), 56);
    assert_eq!(size_of::<BulkResult>(), 80);
    assert_eq!(size_of::<BulkMetadata>(), 72);
    assert_eq!(offset_of!(BulkBand, validity), 40);
    assert_eq!(offset_of!(BulkBand, nodata), 64);
    assert_eq!(offset_of!(BulkBand, band_id), 72);
    assert_eq!(offset_of!(BulkWindow, selection), 16);
    assert_eq!(offset_of!(BulkRequest, windows), 16);
    assert_eq!(offset_of!(BulkResult, valid_count), 40);
    assert_eq!(offset_of!(BulkMetadata, validation_ns), 56);
}
#[test]
fn forty_bands_keep_identity_and_original_dtype() {
    let values = [1.25_f32, 2.5, 3.75];
    let bands = (0..40)
        .map(|i| f32_band(&values, 100 + i))
        .collect::<Vec<_>>();
    let windows = [window(&bands, all())];
    let (code, result, metadata, _) = call(&request(&windows, HM_ORDERED));
    assert_eq!(code, OK);
    for (i, value) in result[..40].iter().enumerate() {
        assert_eq!(value.band_id, 100 + i as u32);
        assert_eq!(value.sum, 7.5);
        assert_eq!(value.valid_count, 3);
        assert_eq!(value.min, 1.25);
        assert_eq!(value.max, 3.75);
        assert_eq!(value.mean, 2.5);
    }
    assert_eq!(metadata.payload_bytes, 40 * 12);
    assert_eq!(metadata.result_bytes, 40 * 80 + 72);
    assert!(metadata.native_owned_bytes < 8192);
}
#[test]
fn strict_and_hm_retain_the_float32_rounding_counterexample() {
    let mut values = vec![16_777_216_f32, 0.5 - 2_f32.powi(-25)];
    values.extend([2_f32.powi(-30); 32]);
    let bands = [f32_band(&values, 1)];
    let windows = [window(&bands, all())];
    let (_, strict, _, _) = call(&request(&windows, STRICT));
    let (_, ordered, _, _) = call(&request(&windows, HM_ORDERED));
    assert_eq!(strict[0].sum, 16_777_216.5);
    assert_eq!(
        ordered[0].sum,
        values.iter().fold(0.0_f64, |s, v| s + *v as f64)
    );
    assert_eq!(ordered[0].sum, 16_777_216.49999997);
    assert_eq!(strict[0].sum.round(), 16_777_217.0);
    assert_eq!(ordered[0].sum.round(), 16_777_216.0);
}
#[test]
fn window_partials_have_their_own_ordered_fold() {
    let left = [10_000_000_000_000_000_f64];
    let right = [1.0_f64, 1.0];
    let mut a = f64_band(&left, 77);
    a.cell_count = left.len() as u64;
    let mut b = f64_band(&right, 77);
    b.cell_count = right.len() as u64;
    let bands_a = [a];
    let bands_b = [b];
    let windows = [window(&bands_a, all()), window(&bands_b, all())];
    let (code, result, _, _) = call(&request(&windows, HM_ORDERED));
    assert_eq!(code, OK);
    assert_eq!(result[0].sum, 10_000_000_000_000_002.0);
    assert_ne!(
        result[0].sum,
        left.iter().chain(&right).fold(0.0, |a, b| a + b)
    );
}
#[test]
fn signed_strict_values_use_existing_stable_sum() {
    let values = [1e16, 1.0, -1e16];
    let bands = [BulkBand {
        cell_count: 3,
        ..f64_band(&values, 0)
    }];
    let windows = [window(&bands, all())];
    let (code, result, _, _) = call(&request(&windows, 0));
    assert_eq!(code, OK);
    assert_eq!(result[0].sum, 1.0);
    assert_eq!(result[0].min, -1e16);
}
#[test]
fn offset_strides_and_bit_mask_skip_invalid_nan_before_reading_values() {
    let values = [99_f32, 1.0, 99.0, f32::NAN, 99.0, 3.0, 99.0, 4.0];
    let mask = [0b0001_0100_u8];
    let bands = [BulkBand {
        byte_offset: 4,
        byte_stride: 8,
        cell_count: 4,
        validity: mask.as_ptr(),
        validity_bytes: 1,
        validity_offset: 2,
        validity_kind: 2,
        ..f32_band(&values, 42)
    }];
    let windows = [window(&bands, all())];
    let (code, result, _, _) = call(&request(&windows, STRICT));
    assert_eq!(code, OK);
    assert_eq!(result[0].sum, 4.0);
    assert_eq!(result[0].valid_count, 2);
    assert_eq!(result[0].excluded_mask, 2);
}
#[test]
fn exclusion_counters_and_zero_follow_explicit_policy() {
    let values = [f32::NAN, f32::INFINITY, -99.0, -2.0, -0.0, 3.0, 50.0];
    let mask = [1_u8, 1, 1, 1, 1, 1, 0];
    let bands = [BulkBand {
        validity: mask.as_ptr(),
        validity_bytes: 7,
        validity_kind: 1,
        nodata: -99.0,
        flags: 1,
        ..f32_band(&values, 3)
    }];
    let windows = [window(&bands, all())];
    let (code, result, _, _) = call(&request(&windows, HM_ORDERED));
    assert_eq!(code, OK);
    assert_eq!(result[0].sum, 3.0);
    assert_eq!(result[0].valid_count, 2);
    assert_eq!(result[0].excluded_mask, 1);
    assert_eq!(result[0].excluded_nodata, 1);
    assert_eq!(result[0].excluded_nonfinite, 2);
    assert_eq!(result[0].excluded_negative, 1);
    assert_eq!(call(&request(&windows, STRICT)).0, NONFINITE);
}
#[test]
fn explicit_nan_nodata_and_zero_nodata_are_excluded() {
    for nodata in [f64::NAN, 0.0] {
        let values = [nodata, 4.0];
        let bands = [BulkBand {
            cell_count: 2,
            nodata,
            flags: 1,
            ..f64_band(&values, 3)
        }];
        let windows = [window(&bands, all())];
        let (code, result, _, _) = call(&request(&windows, STRICT));
        assert_eq!(code, OK);
        assert_eq!(result[0].sum, 4.0);
        assert_eq!(result[0].excluded_nodata, 1);
    }
}
#[test]
fn ordered_index_multiset_and_spans_preserve_repeated_contributions() {
    let values = [1_f32, 2.0, 3.0, 4.0];
    let bands = [f32_band(&values, 9)];
    let indices = [3_u32, 0, 3, 1];
    let spans = [
        BulkSpan { start: 3, count: 1 },
        BulkSpan { start: 0, count: 2 },
        BulkSpan { start: 3, count: 1 },
    ];
    for selected in [selection(&indices, 1), selection(&spans, 3)] {
        let windows = [window(&bands, selected)];
        let (code, result, meta, _) = call(&request(&windows, HM_ORDERED));
        assert_eq!(code, OK);
        assert_eq!(result[0].sum, 11.0);
        assert_eq!(result[0].valid_count, 4);
        assert_eq!(meta.selection_bytes, selected.data_bytes);
    }
}
#[test]
fn u64_indices_and_empty_selection_are_supported() {
    let values = [1_f32, 2.0, 3.0];
    let bands = [f32_band(&values, 1)];
    for indices in [vec![2_u64, 2, 0], vec![]] {
        let windows = [window(&bands, selection(&indices, 2))];
        let (code, result, _, _) = call(&request(&windows, STRICT));
        assert_eq!(code, OK);
        assert_eq!(result[0].sum, if indices.is_empty() { 0.0 } else { 7.0 });
        assert_eq!(result[0].flags, u32::from(!indices.is_empty()));
    }
}
#[test]
fn sum_only_omits_extrema_and_mean_work_and_output() {
    let values = [1_f32, 2.0, 3.0];
    let bands = [f32_band(&values, 1)];
    let windows = [window(&bands, all())];
    let req = BulkRequest {
        reducers: 0,
        ..request(&windows, STRICT)
    };
    let (code, result, _, _) = call(&req);
    assert_eq!(code, OK);
    assert_eq!(result[0].sum, 6.0);
    assert_eq!(
        (result[0].min, result[0].max, result[0].mean),
        (0.0, 0.0, 0.0)
    );
}
#[test]
fn deterministic_random_selections_match_ordered_reference() {
    let mut seed = 0x917ab824_u64;
    for count in 0..96 {
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let values = (0..128)
            .map(|_| (next() % 100_000) as f32 / 8.0)
            .collect::<Vec<_>>();
        let indices = (0..count)
            .map(|_| (next() % 128) as u32)
            .collect::<Vec<_>>();
        let bands = [f32_band(&values, 112)];
        let windows = [window(&bands, selection(&indices, 1))];
        let (code, result, _, _) = call(&request(&windows, HM_ORDERED));
        assert_eq!(code, OK);
        assert_eq!(
            result[0].sum.to_bits(),
            indices
                .iter()
                .fold(0.0_f64, |a, i| a + values[*i as usize] as f64)
                .to_bits()
        );
        assert_eq!(result[0].valid_count, count);
    }
}
#[test]
fn malformed_lengths_offsets_strides_and_masks_fail_atomically() {
    let values = [1_f32, 2.0, 3.0];
    let original = f32_band(&values, 1);
    let cases = [
        (
            BulkBand {
                data_bytes: 4,
                ..original
            },
            "shorter",
        ),
        (
            BulkBand {
                byte_offset: 2,
                ..original
            },
            "misaligned",
        ),
        (
            BulkBand {
                byte_stride: 3,
                ..original
            },
            "stride",
        ),
        (
            BulkBand {
                cell_count: u64::MAX,
                ..original
            },
            "overflow",
        ),
        (
            BulkBand {
                data: ptr::null(),
                ..original
            },
            "null",
        ),
        (
            BulkBand {
                data: unsafe { values.as_ptr().cast::<u8>().add(1) }.cast(),
                ..original
            },
            "misaligned",
        ),
        (
            BulkBand {
                validity_kind: 2,
                validity: values.as_ptr().cast(),
                validity_bytes: 1,
                validity_offset: 7,
                ..original
            },
            "short",
        ),
        (
            BulkBand {
                validity_kind: 1,
                validity_offset: u64::MAX,
                ..original
            },
            "overflow",
        ),
        (
            BulkBand {
                flags: 2,
                ..original
            },
            "flags",
        ),
        (
            BulkBand {
                dtype: 9,
                ..original
            },
            "dtype",
        ),
        (
            BulkBand {
                validity_kind: 9,
                ..original
            },
            "encoding",
        ),
    ];
    for (band, error) in cases {
        let bands = [band];
        let windows = [window(&bands, all())];
        rejection(&request(&windows, STRICT), error);
    }
}
#[test]
fn malformed_selections_fail_before_any_value_result() {
    let values = [1_f32, 2.0, 3.0];
    let bands = [f32_band(&values, 1)];
    let indices = [9_u32];
    let spans = [BulkSpan {
        start: u64::MAX,
        count: 1,
    }];
    let cases = [
        (selection(&indices, 1), "index"),
        (
            BulkSelection {
                count: 3,
                ..selection(&indices, 1)
            },
            "short",
        ),
        (selection(&spans, 3), "overflow"),
        (
            BulkSelection {
                count: u64::MAX,
                ..selection(&indices, 1)
            },
            "overflow",
        ),
        (BulkSelection { flags: 2, ..all() }, "flags"),
        (BulkSelection { kind: 19, ..all() }, "encoding"),
        (BulkSelection { count: 2, ..all() }, "extraneous"),
    ];
    for (selected, error) in cases {
        let windows = [window(&bands, selected)];
        rejection(&request(&windows, STRICT), error);
    }
}
#[test]
fn versions_budgets_and_band_identity_are_validated() {
    let values = [1_f32, 2.0];
    let bands = [f32_band(&values, 1)];
    let windows = [window(&bands, all())];
    let req = request(&windows, STRICT);
    for changed in [
        BulkRequest {
            abi_version: 2,
            ..req
        },
        BulkRequest {
            struct_size: 0,
            ..req
        },
        BulkRequest {
            window_count: 4097,
            ..req
        },
        BulkRequest { flags: 1, ..req },
        BulkRequest { policy: 9, ..req },
        BulkRequest {
            reducers: 16,
            ..req
        },
        BulkRequest {
            max_payload_bytes: 4,
            ..req
        },
        BulkRequest {
            max_payload_bytes: MAX_BYTES + 1,
            ..req
        },
        BulkRequest {
            max_contributions: 1,
            ..req
        },
        BulkRequest {
            max_contributions: MAX_CONTRIBUTIONS + 1,
            ..req
        },
    ] {
        assert_eq!(call(&changed).0, INVALID);
    }
    let other_bands = [f32_band(&values, 2)];
    let wrong_order = [window(&bands, all()), window(&other_bands, all())];
    rejection(&request(&wrong_order, STRICT), "identity/order");
    let duplicate = [bands[0], bands[0]];
    rejection(&request(&[window(&duplicate, all())], STRICT), "duplicate");
    let mismatch = [
        bands[0],
        BulkBand {
            cell_count: 1,
            band_id: 2,
            ..bands[0]
        },
    ];
    rejection(&request(&[window(&mismatch, all())], STRICT), "cell counts");
}
#[test]
fn overflow_is_error_even_for_hm_and_no_partial_band_is_exposed() {
    let good = [2.0_f64];
    let bad = [f64::MAX, f64::MAX];
    let bands = [BulkBand {
        cell_count: 2,
        ..f64_band(&bad, 1)
    }];
    let windows = [window(&bands, all())];
    for policy in [STRICT, HM_ORDERED] {
        assert_eq!(call(&request(&windows, policy)).0, NONFINITE);
    }
    let bands = [
        BulkBand {
            cell_count: 1,
            ..f64_band(&good, 1)
        },
        BulkBand {
            cell_count: 1,
            ..f64_band(&[f64::INFINITY], 2)
        },
    ];
    assert_eq!(
        call(&request(&[window(&bands, all())], STRICT)).0,
        NONFINITE
    );
}
#[test]
fn raw_ffi_null_capacity_and_error_buffer_are_bounded() {
    let values = [1_f32];
    let bands = [f32_band(&values, 1)];
    let windows = [window(&bands, all())];
    let req = request(&windows, STRICT);
    let handle = re_new();
    let mut output = BulkResult::default();
    let mut meta = BulkMetadata::default();
    let mut error = [77_u8; 1];
    unsafe {
        assert_eq!(
            re_bulk(
                handle,
                &req,
                &mut output,
                0,
                &mut meta,
                error.as_mut_ptr(),
                1
            ),
            INVALID
        );
        assert_eq!(error, [0]);
        assert_eq!(
            re_bulk(
                handle,
                ptr::null(),
                &mut output,
                1,
                &mut meta,
                ptr::null_mut(),
                0
            ),
            INVALID
        );
        assert_eq!(
            re_bulk(
                handle,
                &req,
                ptr::null_mut(),
                1,
                &mut meta,
                ptr::null_mut(),
                0
            ),
            INVALID
        );
        assert_eq!(
            re_bulk(
                ptr::null_mut(),
                &req,
                &mut output,
                1,
                &mut meta,
                ptr::null_mut(),
                0
            ),
            INVALID
        );
        assert_eq!(
            re_bulk(handle, &req, &mut output, 1, &mut meta, ptr::null_mut(), 2),
            INVALID
        );
        re_drop(handle);
    }
}
