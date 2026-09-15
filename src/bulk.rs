//! Versioned transient selected-window bulk ABI. See include/skarve_bulk.h.
//! The immutable-allocation precondition belongs to the raw caller; validated
//! lengths never establish that an arbitrary address is a valid allocation.
use crate::{Handle, model::Sum};
use std::{
    ffi::c_void,
    mem::{align_of, size_of},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

pub const ABI: u32 = 1;
pub const MAX_BANDS: usize = 64;
pub const MAX_WINDOWS: u64 = 4096;
pub const MAX_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_CONTRIBUTIONS: u64 = 268_435_456;
pub const STRICT: u32 = 1;
pub const HM_ORDERED: u32 = 2;
pub const SUM: u32 = 1;
pub const MIN: u32 = 2;
pub const MAX: u32 = 4;
pub const MEAN: u32 = 8;
pub const OK: i32 = 0;
pub const INVALID: i32 = 1;
pub const BUSY: i32 = 2;
pub const CANCELLED: i32 = 3;
pub const NONFINITE: i32 = 4;
pub const PANIC: i32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BulkBand {
    pub data: *const c_void,
    pub data_bytes: u64,
    pub byte_offset: u64,
    pub byte_stride: u64,
    pub cell_count: u64,
    pub validity: *const u8,
    pub validity_bytes: u64,
    pub validity_offset: u64,
    pub nodata: f64,
    pub band_id: u32,
    pub dtype: u32,
    pub validity_kind: u32,
    pub flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BulkSpan {
    pub start: u64,
    pub count: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BulkSelection {
    pub data: *const c_void,
    pub data_bytes: u64,
    pub count: u64,
    pub kind: u32,
    pub flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BulkWindow {
    pub bands: *const BulkBand,
    pub band_count: u32,
    pub flags: u32,
    pub selection: BulkSelection,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BulkRequest {
    pub abi_version: u32,
    pub struct_size: u32,
    pub policy: u32,
    pub reducers: u32,
    pub windows: *const BulkWindow,
    pub window_count: u64,
    pub max_payload_bytes: u64,
    pub max_contributions: u64,
    pub flags: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BulkResult {
    pub band_id: u32,
    pub flags: u32,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub valid_count: u64,
    pub excluded_mask: u64,
    pub excluded_nodata: u64,
    pub excluded_nonfinite: u64,
    pub excluded_negative: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BulkMetadata {
    pub abi_version: u32,
    pub struct_size: u32,
    pub window_count: u64,
    pub band_count: u64,
    pub payload_bytes: u64,
    pub selection_bytes: u64,
    pub result_bytes: u64,
    pub native_owned_bytes: u64,
    pub validation_ns: u64,
    pub reduction_ns: u64,
}
#[derive(Debug)]
struct Fault(i32, &'static str);
type Result<T> = std::result::Result<T, Fault>;
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Fault(INVALID, message))
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Fault(CANCELLED, "cancelled"))
    } else {
        Ok(())
    }
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or(Fault(INVALID, "length addition overflow"))
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b)
        .ok_or(Fault(INVALID, "length multiplication overflow"))
}
fn allocation(ptr: *const c_void, bytes: u64, alignment: usize) -> Result<()> {
    require(
        bytes <= isize::MAX as u64,
        "allocation length exceeds addressable size",
    )?;
    require(
        bytes == 0 || !ptr.is_null(),
        "null allocation with nonzero length",
    )?;
    require(
        (ptr as usize).is_multiple_of(alignment),
        "misaligned allocation",
    )?;
    require(
        (ptr as usize).checked_add(bytes as usize).is_some(),
        "allocation address overflow",
    )
}
fn descriptors<T>(ptr: *const T, count: u64) -> Result<()> {
    require(count == 0 || !ptr.is_null(), "null descriptor allocation")?;
    allocation(
        ptr.cast(),
        mul(count, size_of::<T>() as u64)?,
        align_of::<T>(),
    )
}
fn cap(requested: u64, hard: u64) -> Result<u64> {
    require(requested <= hard, "requested budget exceeds fixed cap")?;
    Ok(if requested == 0 { hard } else { requested })
}
fn band_valid(b: &BulkBand) -> Result<()> {
    require(b.flags & !1 == 0, "unknown band flags")?;
    let scalar = match b.dtype {
        1 => 4,
        2 => 8,
        _ => return Err(Fault(INVALID, "unknown value dtype")),
    };
    allocation(b.data, b.data_bytes, scalar as usize)?;
    require(
        b.byte_stride >= scalar && b.byte_stride % scalar == 0,
        "invalid value stride",
    )?;
    require(b.byte_offset % scalar == 0, "misaligned value offset")?;
    let end = if b.cell_count == 0 {
        b.byte_offset
    } else {
        add(
            add(b.byte_offset, mul(b.cell_count - 1, b.byte_stride)?)?,
            scalar,
        )?
    };
    require(
        end <= b.data_bytes,
        "value allocation shorter than offset/stride/count",
    )?;
    match b.validity_kind {
        0 => require(
            b.validity.is_null() && b.validity_bytes == 0 && b.validity_offset == 0,
            "all-valid descriptor has a mask",
        ),
        1 | 2 => {
            allocation(b.validity.cast(), b.validity_bytes, 1)?;
            let end = add(b.validity_offset, b.cell_count)?;
            let required = if b.validity_kind == 1 {
                end
            } else {
                add(end, 7)? / 8
            };
            require(
                required <= b.validity_bytes,
                "validity allocation too short",
            )
        }
        _ => Err(Fault(INVALID, "unknown validity encoding")),
    }
}
fn selection_valid(s: &BulkSelection, cells: u64, cancel: &AtomicBool) -> Result<u64> {
    require(s.flags == 0, "unknown selection flags")?;
    let width = match s.kind {
        0 => {
            require(
                s.data.is_null() && s.data_bytes == 0 && s.count == 0,
                "all selection has extraneous data",
            )?;
            return Ok(cells);
        }
        1 => 4,
        2 => 8,
        3 => 16,
        _ => return Err(Fault(INVALID, "unknown selection encoding")),
    };
    allocation(s.data, s.data_bytes, if width == 4 { 4 } else { 8 })?;
    require(
        mul(s.count, width)? <= s.data_bytes,
        "selection allocation too short",
    )?;
    let mut selected = 0;
    for n in 0..s.count {
        if n % 1024 == 0 {
            cancelled(cancel)?;
        }
        // Allocation and element bounds were checked above. The caller owns
        // the allocation/lifetime/immutability precondition.
        unsafe {
            match s.kind {
                1 => {
                    require(
                        (*s.data.cast::<u32>().add(n as usize) as u64) < cells,
                        "selected index out of bounds",
                    )?;
                    selected = add(selected, 1)?;
                }
                2 => {
                    require(
                        *s.data.cast::<u64>().add(n as usize) < cells,
                        "selected index out of bounds",
                    )?;
                    selected = add(selected, 1)?;
                }
                3 => {
                    let span = &*s.data.cast::<BulkSpan>().add(n as usize);
                    require(
                        add(span.start, span.count)? <= cells,
                        "selected span out of bounds",
                    )?;
                    selected = add(selected, span.count)?;
                }
                _ => unreachable!(),
            }
        }
        require(
            selected <= MAX_CONTRIBUTIONS,
            "selection work budget exceeded",
        )?;
    }
    Ok(selected)
}
struct Validated {
    band_count: usize,
    payload_bytes: u64,
    selection_bytes: u64,
}
fn validate(req: &BulkRequest, capacity: u64, cancel: &AtomicBool) -> Result<Validated> {
    require(
        req.abi_version == ABI && req.struct_size as usize == size_of::<BulkRequest>(),
        "unsupported bulk ABI/version/size",
    )?;
    require(
        req.flags == 0 && req.policy <= HM_ORDERED,
        "unknown request flags or policy",
    )?;
    require(
        req.reducers & !(SUM | MIN | MAX | MEAN) == 0,
        "unknown reducer",
    )?;
    require(
        req.window_count > 0 && req.window_count <= MAX_WINDOWS,
        "window count outside cap",
    )?;
    let bytes_cap = cap(req.max_payload_bytes, MAX_BYTES)?;
    let work_cap = cap(req.max_contributions, MAX_CONTRIBUTIONS)?;
    descriptors(req.windows, req.window_count)?;
    let windows = unsafe { std::slice::from_raw_parts(req.windows, req.window_count as usize) };
    let bands = windows[0].band_count as usize;
    require(
        bands > 0 && bands <= MAX_BANDS && capacity >= bands as u64,
        "band count or result capacity outside bounds",
    )?;
    let mut ids = [0; MAX_BANDS];
    let mut payload = 0;
    let mut selections = 0;
    let mut work = 0;
    for (wi, w) in windows.iter().enumerate() {
        cancelled(cancel)?;
        require(
            w.flags == 0 && w.band_count as usize == bands,
            "window flags or band count mismatch",
        )?;
        descriptors(w.bands, bands as u64)?;
        let band_rows = unsafe { std::slice::from_raw_parts(w.bands, bands) };
        let cells = band_rows[0].cell_count;
        for (bi, b) in band_rows.iter().enumerate() {
            band_valid(b)?;
            require(b.cell_count == cells, "window band cell counts differ")?;
            if wi == 0 {
                require(!ids[..bi].contains(&b.band_id), "duplicate band identity")?;
                ids[bi] = b.band_id;
            } else {
                require(
                    ids[bi] == b.band_id,
                    "band identity/order differs across windows",
                )?;
            }
            payload = add(payload, add(b.data_bytes, b.validity_bytes)?)?;
            require(payload <= bytes_cap, "bulk payload byte budget exceeded")?;
        }
        payload = add(payload, w.selection.data_bytes)?;
        selections = add(selections, w.selection.data_bytes)?;
        require(payload <= bytes_cap, "bulk payload byte budget exceeded")?;
        let selected = selection_valid(&w.selection, cells, cancel)?;
        work = add(work, mul(selected, bands as u64)?)?;
        require(work <= work_cap, "bulk contribution budget exceeded")?;
    }
    Ok(Validated {
        band_count: bands,
        payload_bytes: payload,
        selection_bytes: selections,
    })
}
/// Iterates already validated immutable selection data, without a per-cell
/// allocation or materialization of masks, values or selected indices.
unsafe fn visit(
    s: &BulkSelection,
    cells: u64,
    cancel: &AtomicBool,
    mut callback: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    unsafe {
        match s.kind {
            0 => {
                for index in 0..cells {
                    callback(index)?;
                }
            }
            1 => {
                for n in 0..s.count {
                    callback(*s.data.cast::<u32>().add(n as usize) as u64)?;
                }
            }
            2 => {
                for n in 0..s.count {
                    callback(*s.data.cast::<u64>().add(n as usize))?;
                }
            }
            3 => {
                for n in 0..s.count {
                    if n % 1024 == 0 {
                        cancelled(cancel)?;
                    }
                    let span = &*s.data.cast::<BulkSpan>().add(n as usize);
                    for index in span.start..span.start + span.count {
                        callback(index)?;
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}
unsafe fn reduce<const HM: bool>(
    req: &BulkRequest,
    validated: &Validated,
    cancel: &AtomicBool,
) -> Result<Vec<BulkResult>> {
    let reducers = if req.reducers == 0 { SUM } else { req.reducers };
    let do_min = reducers & MIN != 0;
    let do_max = reducers & MAX != 0;
    let do_sum = reducers & (SUM | MEAN) != 0;
    let windows = unsafe { std::slice::from_raw_parts(req.windows, req.window_count as usize) };
    let mut output = Vec::with_capacity(validated.band_count);
    for bi in 0..validated.band_count {
        let mut result = BulkResult::default();
        let mut stable = Sum::default();
        let mut ordered = 0.0;
        let mut visited = 0_u64;
        for window in windows {
            cancelled(cancel)?;
            let b = unsafe { &*window.bands.add(bi) };
            result.band_id = b.band_id;
            let mut partial = 0.0;
            unsafe {
                visit(&window.selection, b.cell_count, cancel, |index| {
                    if visited % 1024 == 0 {
                        cancelled(cancel)?;
                    }
                    visited += 1;
                    let mask_index = b.validity_offset + index;
                    let valid = match b.validity_kind {
                        0 => true,
                        1 => *b.validity.add(mask_index as usize) != 0,
                        2 => {
                            *b.validity.add((mask_index / 8) as usize) & (1 << (mask_index % 8))
                                != 0
                        }
                        _ => unreachable!(),
                    };
                    if !valid {
                        result.excluded_mask += 1;
                        return Ok(());
                    }
                    let ptr = b
                        .data
                        .cast::<u8>()
                        .add((b.byte_offset + index * b.byte_stride) as usize);
                    let value = if b.dtype == 1 {
                        *ptr.cast::<f32>() as f64
                    } else {
                        *ptr.cast::<f64>()
                    };
                    if b.flags & 1 != 0
                        && (value == b.nodata || (value.is_nan() && b.nodata.is_nan()))
                    {
                        result.excluded_nodata += 1;
                        return Ok(());
                    }
                    if !value.is_finite() {
                        if HM {
                            result.excluded_nonfinite += 1;
                            return Ok(());
                        }
                        return Err(Fault(NONFINITE, "strict selected value is nonfinite"));
                    }
                    if HM && value < 0.0 {
                        result.excluded_negative += 1;
                        return Ok(());
                    }
                    if do_min && (result.valid_count == 0 || value < result.min) {
                        result.min = value;
                    }
                    if do_max && (result.valid_count == 0 || value > result.max) {
                        result.max = value;
                    }
                    result.valid_count += 1;
                    if do_sum {
                        if HM {
                            // Ordinary IEEE binary64 left fold. Rust/LLVM does
                            // not enable fast-math/reassociation for this add.
                            partial += value;
                        } else {
                            stable.add(value);
                        }
                    }
                    Ok(())
                })?;
            }
            if HM && do_sum {
                if !partial.is_finite() {
                    return Err(Fault(NONFINITE, "ordered window sum overflow"));
                }
                ordered += partial;
            }
        }
        let sum = if HM { ordered } else { stable.value() };
        if !sum.is_finite() {
            return Err(Fault(NONFINITE, "selected sum overflow"));
        }
        if reducers & SUM != 0 {
            result.sum = sum;
        }
        if result.valid_count > 0 {
            result.flags = 1;
            if reducers & MEAN != 0 {
                result.mean = sum / result.valid_count as f64;
            }
        }
        output.push(result);
    }
    cancelled(cancel)?;
    Ok(output)
}
fn nanos(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}
fn reset_before_bulk(handle: &Handle, generation: u64) {
    // A cancellation arriving after bulk entry must survive the reset of a
    // completed request's sticky flag. re_cancel publishes its generation
    // before setting the flag, so either this comparison or that store wins.
    handle.cancel.store(false, Ordering::Release);
    if handle.cancel_generation.load(Ordering::Acquire) != generation {
        handle.cancel.store(true, Ordering::Release);
    }
}
unsafe fn invoke(
    handle: *mut Handle,
    request: *const BulkRequest,
    output: *mut BulkResult,
    capacity: u64,
    metadata: *mut BulkMetadata,
) -> Result<()> {
    require(!handle.is_null(), "null handle")?;
    descriptors(handle, 1)?;
    // Match the ordinary API session lock and cancellation lifecycle.
    let handle = unsafe { &*handle };
    let generation = handle.cancel_generation.load(Ordering::Acquire);
    descriptors(request, 1)?;
    descriptors(metadata, 1)?;
    let _session = handle
        .session
        .try_lock()
        .map_err(|_| Fault(BUSY, "session busy or poisoned"))?;
    reset_before_bulk(handle, generation);
    let started = Instant::now();
    let request = unsafe { &*request };
    let checked = validate(request, capacity, &handle.cancel)?;
    descriptors(output, checked.band_count as u64)?;
    let validation_ns = nanos(started);
    let reduction_started = Instant::now();
    let results = if request.policy == HM_ORDERED {
        unsafe { reduce::<true>(request, &checked, &handle.cancel)? }
    } else {
        unsafe { reduce::<false>(request, &checked, &handle.cancel)? }
    };
    let meta = BulkMetadata {
        abi_version: ABI,
        struct_size: size_of::<BulkMetadata>() as u32,
        window_count: request.window_count,
        band_count: checked.band_count as u64,
        payload_bytes: checked.payload_bytes,
        selection_bytes: checked.selection_bytes,
        result_bytes: (results.len() * size_of::<BulkResult>() + size_of::<BulkMetadata>()) as u64,
        native_owned_bytes: (results.capacity() * size_of::<BulkResult>()
            + size_of::<BulkResult>()
            + size_of::<Sum>()
            + MAX_BANDS * size_of::<u32>()) as u64,
        validation_ns,
        reduction_ns: nanos(reduction_started),
    };
    // Commit only after every requested band succeeds. The raw caller promises
    // that these pages do not alias input or each other.
    unsafe {
        std::ptr::copy_nonoverlapping(results.as_ptr(), output, results.len());
        metadata.write(meta);
    }
    Ok(())
}
/// # Safety
/// All pointers must satisfy the allocation, immutability, alignment, no-alias,
/// lifetime and handle rules documented in include/skarve_bulk.h. Invalid
/// addresses cannot be made safe by catch_unwind. Concurrent re_drop is invalid.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn re_bulk(
    handle: *mut Handle,
    request: *const BulkRequest,
    output: *mut BulkResult,
    capacity: u64,
    metadata: *mut BulkMetadata,
    error: *mut u8,
    error_capacity: u64,
) -> i32 {
    // Check this separately so malformed capacity cannot make error handling
    // itself perform a huge or wrapping write. A caller-owned error is optional.
    if error_capacity > 4096
        || (error_capacity > 0 && error.is_null())
        || (error as usize)
            .checked_add(error_capacity as usize)
            .is_none()
    {
        return INVALID;
    }
    let outcome =
        std::panic::catch_unwind(|| unsafe { invoke(handle, request, output, capacity, metadata) })
            .unwrap_or(Err(Fault(PANIC, "native panic contained")));
    let (status, text) = match outcome {
        Ok(()) => (OK, ""),
        Err(Fault(code, text)) => (code, text),
    };
    if error_capacity > 0 {
        let count = text.len().min(error_capacity as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), error, count);
            error.add(count).write(0);
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{re_cancel, re_drop, re_new};
    use std::ptr;

    fn band(values: &[f32]) -> BulkBand {
        BulkBand {
            data: values.as_ptr().cast(),
            data_bytes: std::mem::size_of_val(values) as u64,
            byte_offset: 0,
            byte_stride: 4,
            cell_count: values.len() as u64,
            validity: ptr::null(),
            validity_bytes: 0,
            validity_offset: 0,
            nodata: 0.0,
            band_id: 5,
            dtype: 1,
            validity_kind: 0,
            flags: 0,
        }
    }
    fn request(windows: &[BulkWindow]) -> BulkRequest {
        BulkRequest {
            abi_version: ABI,
            struct_size: size_of::<BulkRequest>() as u32,
            policy: HM_ORDERED,
            reducers: SUM,
            windows: windows.as_ptr(),
            window_count: windows.len() as u64,
            max_payload_bytes: 0,
            max_contributions: 0,
            flags: 0,
        }
    }
    fn window(bands: &[BulkBand]) -> BulkWindow {
        BulkWindow {
            bands: bands.as_ptr(),
            band_count: bands.len() as u32,
            flags: 0,
            selection: BulkSelection {
                data: ptr::null(),
                data_bytes: 0,
                count: 0,
                kind: 0,
                flags: 0,
            },
        }
    }
    #[test]
    fn bulk_and_json_share_one_busy_lock() {
        let handle = re_new();
        let guard = unsafe { &*handle }.session.lock().unwrap();
        let values = [1_f32];
        let bands = [band(&values)];
        let windows = [window(&bands)];
        let req = request(&windows);
        let mut result = BulkResult::default();
        let mut meta = BulkMetadata::default();
        assert_eq!(
            unsafe { re_bulk(handle, &req, &mut result, 1, &mut meta, ptr::null_mut(), 0) },
            BUSY
        );
        assert_eq!(result, BulkResult::default());
        drop(guard);
        unsafe {
            re_drop(handle);
        }
    }
    #[test]
    fn pre_cancelled_validation_and_reduction_return_no_page() {
        let values = [1_f32, 2.0];
        let bands = [band(&values)];
        let windows = [window(&bands)];
        let req = request(&windows);
        let cancel = AtomicBool::new(false);
        let validated = validate(&req, 1, &cancel).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(validate(&req, 1, &cancel).err().unwrap().0, CANCELLED);
        assert_eq!(
            unsafe { reduce::<true>(&req, &validated, &cancel) }
                .err()
                .unwrap()
                .0,
            CANCELLED
        );
    }
    #[test]
    fn cancellation_between_entry_and_reset_survives() {
        let handle = re_new();
        let h = unsafe { &*handle };
        let generation = h.cancel_generation.load(Ordering::Acquire);
        unsafe {
            re_cancel(handle);
        }
        reset_before_bulk(h, generation);
        assert!(h.cancel.load(Ordering::Acquire));
        let generation = h.cancel_generation.load(Ordering::Acquire);
        reset_before_bulk(h, generation);
        assert!(!h.cancel.load(Ordering::Acquire));
        unsafe {
            re_drop(handle);
        }
    }
    #[test]
    fn live_bulk_call_can_be_cancelled_and_handle_reused() {
        // A real owned 16 MiB allocation reused for 8 logical bands. The
        // conservative payload accounting is exactly the 128 MiB cap.
        // No invalid pointers or concurrent drop are exercised.
        let handle = re_new();
        let address = handle as usize;
        let worker = std::thread::spawn(move || {
            let values = vec![1_f32; 4_194_304];
            let bands = (0..8)
                .map(|i| BulkBand {
                    band_id: i,
                    ..band(&values)
                })
                .collect::<Vec<_>>();
            let windows = [window(&bands)];
            let req = request(&windows);
            let sentinel = BulkResult {
                sum: 111.0,
                ..Default::default()
            };
            let mut result = [sentinel; 8];
            let mut meta = BulkMetadata::default();
            // The observer below briefly owns the mutex while probing entry.
            // If it wins that race, the public API correctly returns BUSY.
            // Retry admission rather than mistaking that observer-induced
            // contention for a cancellation failure.
            let admission_start = Instant::now();
            let status = loop {
                let status = unsafe {
                    re_bulk(
                        address as *mut Handle,
                        &req,
                        result.as_mut_ptr(),
                        8,
                        &mut meta,
                        ptr::null_mut(),
                        0,
                    )
                };
                if status != BUSY {
                    break status;
                }
                assert_eq!(result, [sentinel; 8]);
                assert!(admission_start.elapsed().as_secs() < 20);
                std::thread::yield_now();
            };
            (status, result, sentinel)
        });
        let start = Instant::now();
        while unsafe { &*handle }.session.try_lock().is_ok() && !worker.is_finished() {
            assert!(
                start.elapsed().as_secs() < 20,
                "worker failed to enter bulk call"
            );
            std::thread::yield_now();
        }
        unsafe {
            re_cancel(handle);
        }
        let (status, result, sentinel) = worker.join().unwrap();
        assert_eq!(status, CANCELLED);
        assert_eq!(result, [sentinel; 8]);
        let values = [2_f32];
        let bands = [band(&values)];
        let windows = [window(&bands)];
        let mut result = BulkResult::default();
        let mut meta = BulkMetadata::default();
        assert_eq!(
            unsafe {
                re_bulk(
                    handle,
                    &request(&windows),
                    &mut result,
                    1,
                    &mut meta,
                    ptr::null_mut(),
                    0,
                )
            },
            OK
        );
        assert_eq!(result.sum, 2.0);
        unsafe {
            re_drop(handle);
        }
    }
}
