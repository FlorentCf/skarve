//! Exact byte packing for the optional grouped-row SKV representation.
use crate::{
    model::{Band, check_cancel},
    source::{RawBandMetadata, RawBandWindow},
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, write::ZlibEncoder};
use std::{
    cell::Cell,
    io::{self, Write},
    sync::atomic::AtomicBool,
};

/// Enough for 128x128x40 F32 samples and masks, while two packet buffers and
/// the existing per-band inverse remain within the fixed 8 MiB scratch reserve.
pub(super) const MAX_BYTES: usize = 3_276_800;

pub(super) fn shape(
    width: usize,
    height: usize,
    scalar: usize,
    count: usize,
) -> Result<(usize, usize)> {
    ensure!(
        (1..=256).contains(&width)
            && (1..=256).contains(&height)
            && [1, 2, 4, 8].contains(&scalar)
            && (1..=64).contains(&count),
        "invalid SKV group shape"
    );
    let cells = width
        .checked_mul(height)
        .context("SKV group cell overflow")?;
    let band = cells
        .checked_mul(scalar + 1)
        .context("SKV group band overflow")?;
    ensure!(
        band.checked_mul(count).is_some_and(|n| n <= MAX_BYTES),
        "SKV group exceeds decoded packet bound"
    );
    Ok((cells, band))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert(
    packed: &mut [u8],
    band: &[u8],
    member: usize,
    count: usize,
    width: usize,
    height: usize,
    scalar: usize,
    planes: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let (cells, band_bytes) = shape(width, height, scalar, count)?;
    ensure!(
        member < count && packed.len() == band_bytes * count && band.len() == band_bytes,
        "SKV group member length mismatch"
    );
    let group_samples = cells * scalar * count;
    for row in 0..height {
        check_cancel(cancel)?;
        for plane in 0..scalar {
            for x in 0..width {
                let cell = row * width + x;
                let input = if planes {
                    plane * cells + cell
                } else {
                    cell * scalar + plane
                };
                let output = ((row * scalar + plane) * width + x) * count + member;
                packed[output] = band[input];
            }
        }
        for x in 0..width {
            let cell = row * width + x;
            packed[group_samples + cell * count + member] = band[cells * scalar + cell];
        }
    }
    check_cancel(cancel)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn extract(
    packed: &[u8],
    member: usize,
    count: usize,
    width: usize,
    height: usize,
    scalar: usize,
    planes: bool,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let (cells, band_bytes) = shape(width, height, scalar, count)?;
    ensure!(
        member < count && packed.len() == band_bytes * count,
        "SKV group member length mismatch"
    );
    let mut band = vec![0; band_bytes];
    let group_samples = cells * scalar * count;
    for row in 0..height {
        check_cancel(cancel)?;
        for plane in 0..scalar {
            for x in 0..width {
                let cell = row * width + x;
                let output = if planes {
                    plane * cells + cell
                } else {
                    cell * scalar + plane
                };
                let input = ((row * scalar + plane) * width + x) * count + member;
                band[output] = packed[input];
            }
        }
        for x in 0..width {
            let cell = row * width + x;
            band[cells * scalar + cell] = packed[group_samples + cell * count + member];
        }
    }
    check_cancel(cancel)?;
    Ok(band)
}

/// Tile-local source bounds, destination offset, and complete output shape.
pub(super) struct CopyRegion {
    pub source: [usize; 4],
    pub target: [usize; 2],
    pub output_shape: [usize; 2],
}

/// Restore only the requested window directly into its pre-admitted output.
/// Horizontal prediction requires the row prefix before the first requested
/// column; no preceding rows, following columns, or other bands are restored.
/// Full-file verification deliberately retains extract + inverse as a control.
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_member_window(
    packed: &[u8],
    member: usize,
    count: usize,
    width: usize,
    height: usize,
    scalar: usize,
    predicted: bool,
    region: CopyRegion,
    output: &mut RawBandWindow,
    cancel: &AtomicBool,
) -> Result<usize> {
    check_cancel(cancel)?;
    let (cells, band_bytes) = shape(width, height, scalar, count)?;
    let [x0, y0, x1, y1] = region.source;
    let [tx, ty] = region.target;
    let [ow, oh] = region.output_shape;
    ensure!(
        member < count
            && packed.len() == band_bytes * count
            && x0 < x1
            && x1 <= width
            && y0 < y1
            && y1 <= height,
        "invalid SKV grouped restore region"
    );
    let output_cells = ow.checked_mul(oh).context("SKV restore output overflow")?;
    ensure!(
        ow > 0
            && oh > 0
            && output_cells.checked_mul(scalar) == Some(output.samples_le.len())
            && output_cells == output.mask.len()
            && tx.checked_add(x1 - x0).is_some_and(|v| v <= ow)
            && ty.checked_add(y1 - y0).is_some_and(|v| v <= oh),
        "invalid SKV grouped restore destination"
    );
    let group_samples = cells * scalar * count;
    for y in y0..y1 {
        check_cancel(cancel)?;
        let target = (ty + y - y0) * ow + tx;
        for plane in 0..scalar {
            let input = (y * scalar + plane) * width * count + member;
            if predicted {
                let mut previous = 0u8;
                // This prefix is required even when its cells are masked or
                // lie outside the requested window. It operates only on bits.
                for x in 0..x0 {
                    previous = previous.wrapping_add(packed[input + x * count]);
                }
                for x in x0..x1 {
                    previous = previous.wrapping_add(packed[input + x * count]);
                    output.samples_le[(target + x - x0) * scalar + plane] = previous;
                }
            } else {
                for x in x0..x1 {
                    output.samples_le[(target + x - x0) * scalar + plane] =
                        packed[input + x * count];
                }
            }
        }
        for x in x0..x1 {
            output.mask[target + x - x0] = packed[group_samples + (y * width + x) * count + member];
        }
    }
    check_cancel(cancel)?;
    Ok((x1 - x0) * (y1 - y0) * (scalar + 1))
}

/// Restore one selected member row into bounded scratch, then normalize into
/// the caller's final native window. The predictor prefix remains bytewise.
/// Samples and interleaved member masks need at most 2048 + 256 stack bytes;
/// neither a complete raw band nor a complete normalized temporary is created.
/// The returned count includes sample and mask writes into these row buffers.
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_member_native_window(
    packed: &[u8],
    member: usize,
    count: usize,
    width: usize,
    height: usize,
    info: &RawBandMetadata,
    predicted: bool,
    region: CopyRegion,
    output: &mut Band,
    cancel: &AtomicBool,
) -> Result<usize> {
    check_cancel(cancel)?;
    let scalar = info.scalar_type.byte_width();
    let (cells, band_bytes) = shape(width, height, scalar, count)?;
    let [x0, y0, x1, y1] = region.source;
    let [tx, ty] = region.target;
    let [ow, oh] = region.output_shape;
    ensure!(
        member < count
            && packed.len() == band_bytes * count
            && x0 < x1
            && x1 <= width
            && y0 < y1
            && y1 <= height,
        "invalid SKV grouped native restore region"
    );
    let output_cells = ow
        .checked_mul(oh)
        .context("SKV native restore output overflow")?;
    ensure!(
        ow > 0
            && oh > 0
            && output_cells == output.values.len()
            && output_cells == output.valid.len()
            && tx.checked_add(x1 - x0).is_some_and(|v| v <= ow)
            && ty.checked_add(y1 - y0).is_some_and(|v| v <= oh),
        "invalid SKV grouped native restore destination"
    );
    let group_samples = cells * scalar * count;
    let columns = x1 - x0;
    let mut samples = [0u8; 256 * 8];
    let mut masks = [0u8; 256];
    for y in y0..y1 {
        check_cancel(cancel)?;
        for plane in 0..scalar {
            let input = (y * scalar + plane) * width * count + member;
            if predicted {
                let mut previous = 0u8;
                // Masked and unselected prefix cells still contribute to the
                // inverse predictor. Never normalize or skip their raw bits.
                for x in 0..x0 {
                    previous = previous.wrapping_add(packed[input + x * count]);
                }
                for x in x0..x1 {
                    previous = previous.wrapping_add(packed[input + x * count]);
                    samples[(x - x0) * scalar + plane] = previous;
                }
            } else {
                for x in x0..x1 {
                    samples[(x - x0) * scalar + plane] = packed[input + x * count];
                }
            }
        }
        for x in x0..x1 {
            masks[x - x0] = packed[group_samples + (y * width + x) * count + member];
        }
        let target = (ty + y - y0) * ow + tx;
        super::native_window::normalize_row(
            &samples[..columns * scalar],
            &masks[..columns],
            info,
            &mut output.values[target..target + columns],
            &mut output.valid[target..target + columns],
            cancel,
        )?;
    }
    check_cancel(cancel)?;
    Ok(columns * (y1 - y0) * (scalar + 1))
}

struct CappedOutput<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
    overflow: &'a Cell<bool>,
}
impl Write for CappedOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.limit)
        {
            self.overflow.set(true);
            return Err(io::Error::other(
                "SKV grouped compression exceeds original byte length",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// No encoded packet is larger than its decoded bytes. The bounded vector is
/// reused for raw fallback; compression never grows a hidden second capacity.
pub(super) fn encode(
    input: &[u8],
    codec: u32,
    level: u32,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, u32)> {
    ensure!(
        !input.is_empty() && input.len() <= MAX_BYTES && codec <= 1 && level <= 9,
        "invalid SKV group compression request"
    );
    check_cancel(cancel)?;
    if codec == 0 {
        return Ok((input.to_vec(), 0));
    }
    let overflow = Cell::new(false);
    let mut output = Vec::with_capacity(input.len());
    let writer = CappedOutput {
        bytes: &mut output,
        limit: input.len(),
        overflow: &overflow,
    };
    let mut encoder = ZlibEncoder::new(writer, Compression::new(level));
    let result = (|| -> Result<()> {
        for part in input.chunks(65_536) {
            check_cancel(cancel)?;
            encoder.write_all(part)?;
        }
        encoder.finish()?;
        Ok(())
    })();
    check_cancel(cancel)?;
    if overflow.get() {
        output.clear();
        output.extend_from_slice(input);
        return Ok((output, 0));
    }
    result?;
    Ok((output, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{RawScalarType, normalize_raw_sample};

    fn native_info(scalar_type: RawScalarType) -> RawBandMetadata {
        RawBandMetadata {
            scalar_type,
            nodata_f64_bits: Some((-9999.0f64).to_bits()),
            scale_f64_bits: 2.0f64.to_bits(),
            offset_f64_bits: (-1.0f64).to_bits(),
            unit: Some("test".into()),
            mask_flags: 0,
            original_band_index: 0,
            description: String::new(),
        }
    }

    #[test]
    fn native_group_restore_matches_raw_interpretation_at_partial_row_edges() {
        let cancel = AtomicBool::new(false);
        for kind in [
            RawScalarType::Byte,
            RawScalarType::Int8,
            RawScalarType::UInt16,
            RawScalarType::Int16,
            RawScalarType::UInt32,
            RawScalarType::Int32,
            RawScalarType::Float32,
            RawScalarType::Float64,
        ] {
            let info = native_info(kind);
            let scalar = kind.byte_width();
            for width in [1, 31, 255, 256] {
                let (height, count) = (3, 3);
                let (cells, band_bytes) = shape(width, height, scalar, count).unwrap();
                let mut original = (0..count)
                    .map(|member| {
                        (0..band_bytes)
                            .map(|i| ((i * 73 + member * 29 + (i >> 8)) & 255) as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                for (member, bytes) in original.iter_mut().enumerate() {
                    for cell in 0..cells {
                        let value = [
                            0.0,
                            -0.0,
                            f64::from_bits(0x7ff8_1234_5678_9abc),
                            f64::INFINITY,
                            f64::NEG_INFINITY,
                            -9999.0,
                            12.25,
                            -7.5,
                        ][(cell + member) % 8];
                        match kind {
                            RawScalarType::Float32 => bytes[cell * scalar..(cell + 1) * scalar]
                                .copy_from_slice(&(value as f32).to_le_bytes()),
                            RawScalarType::Float64 => bytes[cell * scalar..(cell + 1) * scalar]
                                .copy_from_slice(&value.to_le_bytes()),
                            _ => {}
                        }
                        bytes[cells * scalar + cell] = [0, 1, 127, 255][(cell + member) % 4];
                    }
                }
                for predicted in [false, true] {
                    // Independent row/plane/member wire construction, including
                    // masks in their distinct cell/member order.
                    let mut packet = Vec::with_capacity(band_bytes * count);
                    for y in 0..height {
                        for plane in 0..scalar {
                            for x in 0..width {
                                for bytes in &original {
                                    let current = bytes[(y * width + x) * scalar + plane];
                                    let prior = if predicted && x > 0 {
                                        bytes[(y * width + x - 1) * scalar + plane]
                                    } else {
                                        0
                                    };
                                    packet.push(current.wrapping_sub(prior));
                                }
                            }
                        }
                    }
                    for cell in 0..cells {
                        for bytes in &original {
                            packet.push(bytes[cells * scalar + cell]);
                        }
                    }
                    for x0 in [0, width / 2, width - 1] {
                        let columns = width - x0;
                        let (ow, oh) = (columns + 3, 4);
                        for member in [2, 0, 1] {
                            let mut output = Band {
                                values: vec![-123.25; ow * oh],
                                valid: vec![true; ow * oh],
                                unit: info.unit.clone(),
                            };
                            let copied = restore_member_native_window(
                                &packet,
                                member,
                                count,
                                width,
                                height,
                                &info,
                                predicted,
                                CopyRegion {
                                    source: [x0, 1, width, height],
                                    target: [2, 1],
                                    output_shape: [ow, oh],
                                },
                                &mut output,
                                &cancel,
                            )
                            .unwrap();
                            assert_eq!(copied, columns * 2 * (scalar + 1));
                            assert_eq!(output.unit, info.unit);
                            for y in 0..oh {
                                for x in 0..ow {
                                    let expected = if (1..3).contains(&y)
                                        && (2..columns + 2).contains(&x)
                                    {
                                        let cell = y * width + x0 + x - 2;
                                        let bytes =
                                            &original[member][cell * scalar..(cell + 1) * scalar];
                                        macro_rules! scalar {
                                            ($type:ty) => {
                                                <$type>::from_le_bytes(bytes.try_into().unwrap())
                                                    as f64
                                            };
                                        }
                                        let raw = match kind {
                                            RawScalarType::Byte => scalar!(u8),
                                            RawScalarType::Int8 => scalar!(i8),
                                            RawScalarType::UInt16 => scalar!(u16),
                                            RawScalarType::Int16 => scalar!(i16),
                                            RawScalarType::UInt32 => scalar!(u32),
                                            RawScalarType::Int32 => scalar!(i32),
                                            RawScalarType::Float32 => scalar!(f32),
                                            RawScalarType::Float64 => scalar!(f64),
                                        };
                                        normalize_raw_sample(
                                            raw,
                                            original[member][cells * scalar + cell],
                                            Some(-9999.0),
                                            2.0,
                                            -1.0,
                                        )
                                    } else {
                                        (-123.25, true)
                                    };
                                    assert_eq!(
                                        output.values[y * ow + x].to_bits(),
                                        expected.0.to_bits()
                                    );
                                    assert_eq!(output.valid[y * ow + x], expected.1);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn native_group_restore_rejects_invalid_or_cancelled_work_before_writing() {
        let info = native_info(RawScalarType::Float32);
        let mut output = Band {
            values: vec![-123.25; 4],
            valid: vec![true; 4],
            unit: info.unit.clone(),
        };
        for (source, target, shape, member, length, cancelled) in [
            ([0, 0, 2, 2], [0, 0], [2, 2], 0, 20, true),
            ([1, 0, 3, 2], [0, 0], [2, 2], 0, 20, false),
            ([0, 0, 2, 2], [1, 0], [2, 2], 0, 20, false),
            ([0, 0, 2, 2], [usize::MAX, 0], [2, 2], 0, 20, false),
            ([0, 0, 0, 2], [0, 0], [2, 2], 0, 20, false),
            ([0, 0, 2, 2], [0, 0], [usize::MAX, 2], 0, 20, false),
            ([0, 0, 2, 2], [0, 0], [1, 2], 0, 20, false),
            ([0, 0, 2, 2], [0, 0], [2, 2], 1, 20, false),
            ([0, 0, 2, 2], [0, 0], [2, 2], 0, 19, false),
        ] {
            assert!(
                restore_member_native_window(
                    &[0; 20][..length],
                    member,
                    1,
                    2,
                    2,
                    &info,
                    true,
                    CopyRegion {
                        source,
                        target,
                        output_shape: shape,
                    },
                    &mut output,
                    &AtomicBool::new(cancelled),
                )
                .is_err()
            );
            assert_eq!(output.values, [-123.25; 4]);
            assert_eq!(output.valid, [true; 4]);
        }
    }

    #[test]
    fn grouped_byte_packing_preserves_every_component_and_partial_rows() {
        let cancel = AtomicBool::new(false);
        for scalar in [1, 2, 4, 8] {
            for planes in [false, true] {
                let (w, h, count) = (31, 17, 13);
                let (_, n) = shape(w, h, scalar, count).unwrap();
                let input = (0..count)
                    .map(|b| {
                        (0..n)
                            .map(|i| ((i * 73 + b * 29 + (i >> 8)) & 255) as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                let mut packet = vec![0; n * count];
                for (b, values) in input.iter().enumerate() {
                    insert(&mut packet, values, b, count, w, h, scalar, planes, &cancel).unwrap();
                }
                for b in (0..count).rev() {
                    assert_eq!(
                        extract(&packet, b, count, w, h, scalar, planes, &cancel).unwrap(),
                        input[b]
                    );
                }
                assert!(extract(&packet, count, count, w, h, scalar, planes, &cancel).is_err());
            }
        }
    }

    #[test]
    fn group_cap_admits_forty_bands_without_raising_the_scratch_budget() {
        assert_eq!(shape(128, 128, 4, 40).unwrap().1 * 40, MAX_BYTES);
        assert!(shape(128, 128, 4, 41).is_err());
        assert!(shape(256, 256, 4, 11).is_err());
        assert!(shape(usize::MAX, 1, 4, 1).is_err());
        assert!(shape(1, 1, 8, 65).is_err());
    }

    #[test]
    fn bounded_encoding_falls_back_raw_and_observes_cancellation() {
        let cancel = AtomicBool::new(false);
        let (output, codec) = encode(&[123], 1, 3, &cancel).unwrap();
        assert_eq!(codec, 0);
        assert_eq!(output, [123]);
        let (output, codec) = encode(&vec![0; 65536], 1, 3, &cancel).unwrap();
        assert_eq!(codec, 1);
        assert!(output.len() < 65536);
        assert!(encode(&[1], 1, 3, &AtomicBool::new(true)).is_err());
        assert!(extract(&[0; 10], 0, 1, 2, 1, 4, false, &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn fused_restore_preserves_prefixes_masks_bits_and_untouched_destination() {
        let cancel = AtomicBool::new(false);
        let (w, h) = (31, 17);
        for scalar in [1, 2, 4, 8] {
            for count in [1, 3, 40, 64] {
                let (_, band_bytes) = shape(w, h, scalar, count).unwrap();
                let original = (0..count)
                    .map(|b| {
                        (0..band_bytes)
                            .map(|i| ((i * 73 + b * 29 + (i >> 8)) & 255) as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                for predicted in [false, true] {
                    // Independent wire construction avoids relying on insert or
                    // the existing extraction/inverse implementation as oracle.
                    let mut packet = Vec::with_capacity(band_bytes * count);
                    for y in 0..h {
                        for plane in 0..scalar {
                            for x in 0..w {
                                for bytes in &original {
                                    let byte = bytes[(y * w + x) * scalar + plane];
                                    let prior = if predicted && x > 0 {
                                        bytes[(y * w + x - 1) * scalar + plane]
                                    } else {
                                        0
                                    };
                                    packet.push(byte.wrapping_sub(prior));
                                }
                            }
                        }
                    }
                    for cell in 0..w * h {
                        for bytes in &original {
                            packet.push(bytes[w * h * scalar + cell]);
                        }
                    }
                    for [x0, y0, x1, y1] in [
                        [0, 0, w, h],
                        [17, 4, 30, 16],
                        [30, 16, 31, 17],
                        [0, 7, 1, 8],
                    ] {
                        let (ow, oh) = (x1 - x0 + 4, y1 - y0 + 5);
                        for member in [count - 1, 0, count / 2] {
                            let mut output = RawBandWindow {
                                samples_le: vec![0xa5; ow * oh * scalar],
                                mask: vec![0xa5; ow * oh],
                            };
                            let copied = restore_member_window(
                                &packet,
                                member,
                                count,
                                w,
                                h,
                                scalar,
                                predicted,
                                CopyRegion {
                                    source: [x0, y0, x1, y1],
                                    target: [2, 3],
                                    output_shape: [ow, oh],
                                },
                                &mut output,
                                &cancel,
                            )
                            .unwrap();
                            assert_eq!(copied, (x1 - x0) * (y1 - y0) * (scalar + 1));
                            for y in 0..oh {
                                for x in 0..ow {
                                    let inside =
                                        x >= 2 && x < x1 - x0 + 2 && y >= 3 && y < y1 - y0 + 3;
                                    let cell =
                                        (y0 + y.saturating_sub(3)) * w + x0 + x.saturating_sub(2);
                                    for plane in 0..scalar {
                                        assert_eq!(
                                            output.samples_le[(y * ow + x) * scalar + plane],
                                            if inside {
                                                original[member][cell * scalar + plane]
                                            } else {
                                                0xa5
                                            }
                                        );
                                    }
                                    assert_eq!(
                                        output.mask[y * ow + x],
                                        if inside {
                                            original[member][w * h * scalar + cell]
                                        } else {
                                            0xa5
                                        }
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fused_restore_invalid_or_cancelled_work_does_not_touch_destination() {
        let mut output = RawBandWindow {
            samples_le: vec![0xa5; 16],
            mask: vec![0xa5; 4],
        };
        for (source, target, cancelled) in [
            ([0, 0, 2, 2], [0, 0], true),
            ([1, 0, 3, 2], [0, 0], false),
            ([0, 0, 2, 2], [1, 0], false),
            ([0, 0, 2, 2], [usize::MAX, 0], false),
            ([0, 0, 0, 2], [0, 0], false),
        ] {
            assert!(
                restore_member_window(
                    &[0; 20],
                    0,
                    1,
                    2,
                    2,
                    4,
                    true,
                    CopyRegion {
                        source,
                        target,
                        output_shape: [2, 2]
                    },
                    &mut output,
                    &AtomicBool::new(cancelled)
                )
                .is_err()
            );
            assert_eq!(output.samples_le, [0xa5; 16]);
            assert_eq!(output.mask, [0xa5; 4]);
        }
    }
}
