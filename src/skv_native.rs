//! Normalized SKV output construction. Raw typed access remains separate.
use crate::{
    model::{Band, Raster, check_cancel},
    source::{
        RasterMetadata, RawBandMetadata, RawRasterMetadata, RawScalarType, normalize_raw_sample,
    },
};
use anyhow::{Result, ensure};
use std::sync::atomic::AtomicBool;

pub(super) fn allocate(
    metadata: &RasterMetadata,
    raw: &RawRasterMetadata,
    [x, y, w, h]: [usize; 4],
    indices: &[usize],
    cancel: &AtomicBool,
) -> Result<Raster> {
    // Caller has validated indices/window and reserved the unchanged complete
    // raw-plus-normalized read bound before this allocation.
    check_cancel(cancel)?;
    let mut bands = Vec::with_capacity(indices.len());
    for &index in indices {
        let info = &raw.bands[index];
        ensure!(
            f64::from_bits(info.scale_f64_bits).is_finite()
                && f64::from_bits(info.offset_f64_bits).is_finite(),
            "nonfinite typed scale or offset"
        );
        bands.push(Band {
            values: vec![0.; w * h],
            valid: vec![false; w * h],
            unit: info.unit.clone(),
        });
    }
    let mut grid = metadata.grid.clone();
    grid.width = w;
    grid.height = h;
    grid.transform[0] += x as f64 * grid.transform[1];
    grid.transform[3] += y as f64 * grid.transform[5];
    Ok(Raster {
        grid,
        bands,
        source_id: metadata.source_id.clone(),
    })
}

pub(super) fn normalize_row(
    samples: &[u8],
    mask: &[u8],
    info: &RawBandMetadata,
    values: &mut [f64],
    valid: &mut [bool],
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let width = info.scalar_type.byte_width();
    ensure!(
        mask.len() <= 256
            && samples.len() == mask.len() * width
            && values.len() == mask.len()
            && valid.len() == mask.len(),
        "SKV normalized row shape mismatch"
    );
    let nodata = info.nodata_f64_bits.map(f64::from_bits);
    let scale = f64::from_bits(info.scale_f64_bits);
    let offset = f64::from_bits(info.offset_f64_bits);
    // Dispatch once per selected row. The arithmetic/predicate is exactly the
    // shared raw normalizer; no changed order, fast-math or invalid shortcut.
    macro_rules! normalize {
        ($kind:ty) => {
            for (((bytes, &mask), value), valid) in
                samples.chunks_exact(width).zip(mask).zip(values).zip(valid)
            {
                let raw = <$kind>::from_le_bytes(bytes.try_into().expect("validated scalar width"))
                    as f64;
                (*value, *valid) = normalize_raw_sample(raw, mask, nodata, scale, offset);
            }
        };
    }
    match info.scalar_type {
        RawScalarType::Byte => normalize!(u8),
        RawScalarType::Int8 => normalize!(i8),
        RawScalarType::UInt16 => normalize!(u16),
        RawScalarType::Int16 => normalize!(i16),
        RawScalarType::UInt32 => normalize!(u32),
        RawScalarType::Int32 => normalize!(i32),
        RawScalarType::Float32 => normalize!(f32),
        RawScalarType::Float64 => normalize!(f64),
    }
    check_cancel(cancel)
}
