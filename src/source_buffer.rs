//! Bounded original-scalar window output. No normalization or numerical reduction.
use crate::{
    Handle,
    model::check_cancel,
    source::{ReadMetrics, WindowSource},
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char},
    sync::atomic::{AtomicBool, Ordering},
};

pub const MAX_OUTPUT_BYTES: usize = 64 << 20;
pub const MAX_WORKING_BYTES: usize = 128 << 20;
pub const MAX_WINDOW_BANDS: usize = 64;
// Covers descriptor strings, bounded source traces, and JSON serialization copies.
const CONTROL_BYTES: usize = 16 << 20;
fn default_working() -> usize {
    MAX_WORKING_BYTES
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub source: String,
    pub window: [usize; 4],
    pub bands: Vec<usize>,
    #[serde(default = "default_working")]
    pub working_bytes: usize,
}
fn aligned(n: usize) -> Result<usize> {
    Ok(n.checked_add(7)
        .ok_or_else(|| anyhow::anyhow!("window byte overflow"))?
        & !7)
}
/// The caller owns output. Both output and reader scratch are charged to working_bytes.
/// Values and independent masks retain original bytes, including invalid payloads.
pub fn read(
    source: &dyn WindowSource,
    request: &Request,
    output: &mut [u8],
    cancel: &AtomicBool,
) -> Result<Value> {
    check_cancel(cancel)?;
    ensure!(
        output.len() <= MAX_OUTPUT_BYTES
            && request.working_bytes <= MAX_WORKING_BYTES
            && request.working_bytes >= CONTROL_BYTES,
        "window resource budget exceeded"
    );
    let [x, y, width, height] = request.window;
    let metadata = source.metadata();
    ensure!(
        width > 0
            && height > 0
            && x.checked_add(width)
                .is_some_and(|v| v <= metadata.grid.width)
            && y.checked_add(height)
                .is_some_and(|v| v <= metadata.grid.height),
        "window outside source grid"
    );
    let raw = source
        .raw_metadata()
        .ok_or_else(|| anyhow::anyhow!("source has no original scalar interface"))?;
    ensure!(
        !request.bands.is_empty()
            && request.bands.len() <= MAX_WINDOW_BANDS
            && request
                .bands
                .iter()
                .enumerate()
                .all(|(n, &i)| i < raw.bands.len() && !request.bands[..n].contains(&i)),
        "invalid selected window bands"
    );
    let cells = width
        .checked_mul(height)
        .ok_or_else(|| anyhow::anyhow!("window size overflow"))?;
    let mut needed = 0usize;
    let mut bands = Vec::with_capacity(request.bands.len());
    for &index in &request.bands {
        let info = &raw.bands[index];
        let start = aligned(needed)?;
        let bytes = cells
            .checked_mul(info.scalar_type.byte_width())
            .ok_or_else(|| anyhow::anyhow!("window byte overflow"))?;
        let mask = start
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("window byte overflow"))?;
        needed = mask
            .checked_add(cells)
            .ok_or_else(|| anyhow::anyhow!("window byte overflow"))?;
        bands.push(json!({"sourceBand":index,"scalarType":scalar_name(info.scalar_type),"byteOffset":start,"byteLength":bytes,"maskOffset":mask,"maskLength":cells,
            "metadata":{"nodataBits":info.nodata_f64_bits.map(|v|format!("{v:016x}")),"scaleBits":format!("{:016x}",info.scale_f64_bits),"offsetBits":format!("{:016x}",info.offset_f64_bits),
            "maskFlags":info.mask_flags,"originalBandIndex":info.original_band_index,"description":info.description,"unit":info.unit}}));
    }
    ensure!(needed <= output.len(), "output capacity is too small");
    // The complete caller output stays live while one admitted raw group is
    // decoded. Preflight every group before verification or payload I/O; bounds
    // include the reader's positional physical footprint, not just sample bytes.
    let read_available = request
        .working_bytes
        .checked_sub(output.len())
        .and_then(|n| n.checked_sub(CONTROL_BYTES))
        .ok_or_else(|| anyhow::anyhow!("window working budget exceeded"))?;
    let max_group = source.max_raw_read_bands().min(MAX_WINDOW_BANDS);
    ensure!(max_group > 0, "source has no admitted raw band group");
    let mut groups = Vec::new();
    let mut first = 0;
    let mut read_bound = 0;
    while first < request.bands.len() {
        check_cancel(cancel)?;
        let mut end = (first + max_group).min(request.bands.len());
        let bound = loop {
            let bound =
                source.raw_read_buffer_bound_at(x, y, width, height, &request.bands[first..end])?;
            if bound <= read_available {
                break bound;
            }
            ensure!(end > first + 1, "window working budget exceeded");
            end -= 1;
        };
        groups.push((first, end, bound));
        read_bound = read_bound.max(bound);
        first = end;
    }
    let total = output.len() + CONTROL_BYTES + read_bound;
    let mut metrics = ReadMetrics::default();
    source.verify_immutable()?;
    for &(first, end, bound) in &groups {
        check_cancel(cancel)?;
        let (window, read) = source.read_raw_selected_window_cancellable(
            x,
            y,
            width,
            height,
            &request.bands[first..end],
            bound,
            cancel,
        )?;
        ensure!(
            window.width == width && window.height == height && window.bands.len() == end - first,
            "raw reader shape mismatch"
        );
        metrics.read_decode_ms += read.read_decode_ms;
        metrics.normalization_ms += read.normalization_ms;
        metrics.raster_io_calls += read.raster_io_calls;
        metrics.decoder_cache_flushes += read.decoder_cache_flushes;
        metrics.decoder_cache_reused_chunks += read.decoder_cache_reused_chunks;
        for (data, descriptor) in window.bands.iter().zip(&bands[first..end]) {
            let offset = descriptor["byteOffset"].as_u64().unwrap() as usize;
            let bytes = descriptor["byteLength"].as_u64().unwrap() as usize;
            let mask = descriptor["maskOffset"].as_u64().unwrap() as usize;
            ensure!(
                data.samples_le.len() == bytes && data.mask.len() == cells,
                "raw reader payload mismatch"
            );
            // Bounded copies allow cooperative cancellation even on the widest read.
            for (chunk, dest) in data
                .samples_le
                .chunks(65536)
                .zip(output[offset..offset + bytes].chunks_mut(65536))
            {
                check_cancel(cancel)?;
                dest.copy_from_slice(chunk);
            }
            for (chunk, dest) in data
                .mask
                .chunks(65536)
                .zip(output[mask..mask + cells].chunks_mut(65536))
            {
                check_cancel(cancel)?;
                dest.copy_from_slice(chunk);
            }
        }
    }
    // Partial bytes are never a successful result. The caller owns output but
    // must not consume it until this complete operation passes its final check.
    source.verify_immutable()?;
    check_cancel(cancel)?;
    let mut grid = metadata.grid.clone();
    grid.width = width;
    grid.height = height;
    grid.transform[0] += x as f64 * grid.transform[1];
    grid.transform[3] += y as f64 * grid.transform[5];
    Ok(
        json!({"abiVersion":1,"width":width,"height":height,"bands":bands,"byteLength":needed,"grid":grid,
        "identity":source.identity_descriptor(),"diagnostics":source.diagnostics(),"readMetrics":metrics,"rawReadGroups":groups.len(),
        "reservedBytes":{"output":output.len(),"reader":read_bound,"control":CONTROL_BYTES,"working":total,"retainedSource":source.retained_memory_bound()}}),
    )
}

/// # Safety
/// Live handle, immutable NUL-terminated UTF-8 control, and exclusively writable
/// output allocation of capacity bytes must remain alive until return. No drop
/// may race this call. Output is usable only when returned envelope is ok:true.
/// Release returned metadata with re_free_string. No pixel data enters JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn re_read_window(
    handle: *mut Handle,
    input: *const c_char,
    output: *mut u8,
    capacity: u64,
) -> *mut c_char {
    let result = std::panic::catch_unwind(|| -> Result<Value> {
        ensure!(
            !handle.is_null() && !input.is_null() && !output.is_null(),
            "null source window argument"
        );
        ensure!(
            capacity <= MAX_OUTPUT_BYTES as u64,
            "source window capacity exceeds cap"
        );
        let handle = unsafe { &*handle };
        let session = handle
            .session
            .try_lock()
            .map_err(|_| anyhow::anyhow!("session busy or poisoned"))?;
        handle.cancel.store(false, Ordering::Relaxed);
        let input = unsafe { CStr::from_ptr(input) }.to_str()?;
        ensure!(input.len() <= 8192, "source window control exceeds 8 KiB");
        let request: Request = serde_json::from_str(input)?;
        ensure!(
            request.source.len() <= 1024,
            "source identifier exceeds cap"
        );
        let source = session.reader(&request.source)?;
        ensure!(
            request
                .working_bytes
                .checked_add(source.retained_memory_bound())
                .is_some_and(|n| n <= session.available()),
            "insufficient session source window memory"
        );
        let buffer = unsafe { std::slice::from_raw_parts_mut(output, capacity as usize) };
        read(source, &request, buffer, &handle.cancel)
    })
    .unwrap_or_else(|_| Err(anyhow::anyhow!("native panic contained")));
    let envelope = match result {
        Ok(v) => json!({"ok":true,"result":v}),
        Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
    };
    CString::new(envelope.to_string())
        .expect("JSON contains no NUL")
        .into_raw()
}

/// JSON-safe exact raw metadata; u64 bit patterns are hexadecimal strings.
pub fn metadata(source: &dyn WindowSource) -> Value {
    let Some(raw) = source.raw_metadata() else {
        return Value::Null;
    };
    let bands: Vec<Value> = raw.bands.iter().map(|b|json!({"scalarType":scalar_name(b.scalar_type),"nodataBits":b.nodata_f64_bits.map(|n|format!("{n:016x}")),
        "scaleBits":format!("{:016x}",b.scale_f64_bits),"offsetBits":format!("{:016x}",b.offset_f64_bits),"maskFlags":b.mask_flags,
        "originalBandIndex":b.original_band_index,"description":b.description,"unit":b.unit})).collect();
    json!({"bands":bands,"sourceOverview":raw.source_overview,"sourceBandCount":raw.source_band_count,"pixelConvention":raw.pixel_convention,"maxReadBands":source.max_raw_read_bands(),"maxWindowBands":MAX_WINDOW_BANDS})
}

fn scalar_name(value: crate::source::RawScalarType) -> &'static str {
    use crate::source::RawScalarType::*;
    match value {
        Byte => "byte",
        Int8 => "int8",
        UInt16 => "uint16",
        Int16 => "int16",
        UInt32 => "uint32",
        Int32 => "int32",
        Float32 => "float32",
        Float64 => "float64",
    }
}
