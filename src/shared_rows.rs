//! Transient exact dyadic row prefixes and requested-statistic range extrema.
//!
//! This is an opt-in execution helper, not persisted source preparation. A
//! rejected row or range must use the ordinary raw contribution path. Prefix
//! subtraction is integer-exact: a tiny interval never subtracts rounded large
//! floating prefixes. Established fixed-point accumulator / prefix-sum ideas.
use crate::{
    aggregate::Options,
    model::{Band, Sum},
};

const COEFFICIENT_LIMIT: u128 = 1u128 << 100;
/// A row helper must fit this bound as well as its caller's live memory budget.
pub const MAX_ROW_BYTES: usize = 64 * 1024 * 1024;

pub struct RowSummary {
    width: usize,
    exponent: i32,
    sums: Option<Vec<i128>>,
    counts: Vec<usize>,
    min: Option<Vec<f64>>,
    max: Option<Vec<f64>>,
    tree_base: usize,
    bytes: usize,
}
impl RowSummary {
    /// Exact vector capacity plus fixed Rust headers; checked before allocation.
    pub fn estimate_bytes(width: usize, options: &Options) -> Option<usize> {
        if width == 0 || !options.summaries_eligible() {
            return None;
        }
        let entries = width.checked_add(1)?;
        let mut bytes = entries
            .checked_mul(std::mem::size_of::<usize>())?
            .checked_add(std::mem::size_of::<Self>())?;
        if options.needs_sum() {
            bytes = bytes.checked_add(entries.checked_mul(std::mem::size_of::<i128>())?)?;
        }
        let extrema = usize::from(options.needs_min()) + usize::from(options.needs_max());
        if extrema > 0 {
            bytes = bytes.checked_add(
                width
                    .checked_next_power_of_two()?
                    .checked_mul(2)?
                    .checked_mul(std::mem::size_of::<f64>())?
                    .checked_mul(extrema)?,
            )?;
        }
        (bytes <= MAX_ROW_BYTES).then_some(bytes)
    }

    /// `row` is a zero-based row index in a dense `width`-column Band.
    /// The caller polls cancellation between bounded tile rows; this helper
    /// performs at most two value scans plus requested extrema-tree construction.
    pub fn build(band: &Band, row: usize, width: usize, options: &Options) -> Option<Self> {
        Self::build_counted(band, row, width, options, &mut 0)
    }

    /// Count actual input visits, including rejected prefix builds.
    pub fn build_counted(
        band: &Band,
        row: usize,
        width: usize,
        options: &Options,
        visits: &mut u64,
    ) -> Option<Self> {
        let bytes = Self::estimate_bytes(width, options)?;
        let begin = row.checked_mul(width)?;
        let end = begin.checked_add(width)?;
        if band.values.len() != band.valid.len() || end > band.values.len() {
            return None;
        }
        let values = &band.values[begin..end];
        let valid = &band.valid[begin..end];
        let needs_sum = options.needs_sum();
        let mut exponent = i32::MAX;
        if needs_sum {
            for (&value, &present) in values.iter().zip(valid) {
                *visits += 1;
                if present {
                    let (coefficient, e) = dyadic(value)?;
                    if coefficient != 0 {
                        exponent = exponent.min(e);
                    }
                }
            }
        }
        if exponent == i32::MAX {
            exponent = 0;
        }
        let entries = width.checked_add(1)?;
        let mut sums = needs_sum.then(|| vec![0i128; entries]);
        let mut counts = vec![0usize; entries];
        let tree_base = if options.needs_min() || options.needs_max() {
            width.checked_next_power_of_two()?
        } else {
            0
        };
        let mut min = options
            .needs_min()
            .then(|| vec![f64::INFINITY; tree_base * 2]);
        let mut max = options
            .needs_max()
            .then(|| vec![f64::NEG_INFINITY; tree_base * 2]);
        let mut absolute = 0u128;
        for (i, (&value, &present)) in values.iter().zip(valid).enumerate() {
            *visits += 1;
            counts[i + 1] = counts[i];
            if let Some(sums) = &mut sums {
                sums[i + 1] = sums[i];
            }
            if !present {
                continue;
            }
            if !value.is_finite() {
                return None;
            }
            counts[i + 1] += 1;
            if let Some(sums) = &mut sums {
                let (coefficient, e) = dyadic(value)?;
                let aligned = if coefficient == 0 {
                    0
                } else {
                    let shift = u32::try_from(e.checked_sub(exponent)?).ok()?;
                    let magnitude = coefficient.unsigned_abs();
                    let bits = 128 - magnitude.leading_zeros();
                    if bits.checked_add(shift)? > 101 || shift >= 128 {
                        return None;
                    }
                    let magnitude = magnitude.checked_shl(shift)?;
                    absolute = absolute.checked_add(magnitude)?;
                    if absolute > COEFFICIENT_LIMIT {
                        return None;
                    }
                    let aligned = i128::try_from(magnitude).ok()?;
                    if coefficient < 0 { -aligned } else { aligned }
                };
                sums[i + 1] = sums[i].checked_add(aligned)?;
            }
            if let Some(tree) = &mut min {
                tree[tree_base + i] = value;
            }
            if let Some(tree) = &mut max {
                tree[tree_base + i] = value;
            }
        }
        for i in (1..tree_base).rev() {
            if let Some(tree) = &mut min {
                tree[i] = tree[2 * i].min(tree[2 * i + 1]);
            }
            if let Some(tree) = &mut max {
                tree[i] = tree[2 * i].max(tree[2 * i + 1]);
            }
        }
        Some(Self {
            width,
            exponent,
            sums,
            counts,
            min,
            max,
            tree_base,
            bytes,
        })
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn width(&self) -> usize {
        self.width
    }

    /// Half-open column range. Unrequested extrema return empty-state infinities.
    /// An unrepresentable finite compensated sum returns None for raw fallback.
    pub fn range(&self, start: usize, end: usize) -> Option<(Sum, usize, f64, f64)> {
        if start > end || end > self.width {
            return None;
        }
        let sum = if let Some(prefix) = &self.sums {
            exact_sum(prefix[end].checked_sub(prefix[start])?, self.exponent)?
        } else {
            Sum::default()
        };
        let count = self.counts[end].checked_sub(self.counts[start])?;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        if self.tree_base > 0 {
            let (mut a, mut b) = (start + self.tree_base, end + self.tree_base);
            while a < b {
                if a & 1 == 1 {
                    if let Some(tree) = &self.min {
                        min = min.min(tree[a]);
                    }
                    if let Some(tree) = &self.max {
                        max = max.max(tree[a]);
                    }
                    a += 1;
                }
                if b & 1 == 1 {
                    b -= 1;
                    if let Some(tree) = &self.min {
                        min = min.min(tree[b]);
                    }
                    if let Some(tree) = &self.max {
                        max = max.max(tree[b]);
                    }
                }
                a >>= 1;
                b >>= 1;
            }
        }
        Some((sum, count, min, max))
    }
}

/// Canonical signed odd coefficient and binary exponent of a finite f64.
fn dyadic(value: f64) -> Option<(i128, i32)> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.to_bits();
    let field = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mut magnitude, mut exponent) = if field == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), field - 1023 - 52)
    };
    if magnitude == 0 {
        return Some((0, 0));
    }
    let trailing = magnitude.trailing_zeros();
    magnitude >>= trailing;
    exponent += trailing as i32;
    let coefficient = magnitude as i128;
    Some((
        if bits >> 63 != 0 {
            -coefficient
        } else {
            coefficient
        },
        exponent,
    ))
}

/// Construct coefficient * 2^exponent exactly without overflowing/underflowing
/// an intermediate power-of-two multiplication. Coefficient has <=53 odd bits.
fn scaled_integer(coefficient: i128, mut exponent: i32) -> Option<f64> {
    if coefficient == 0 {
        return Some(0.);
    }
    let sign = u64::from(coefficient < 0) << 63;
    let mut magnitude = coefficient.unsigned_abs();
    let trailing = magnitude.trailing_zeros();
    magnitude >>= trailing;
    exponent = exponent.checked_add(trailing as i32)?;
    let bits = 128 - magnitude.leading_zeros();
    if bits > 53 {
        return None;
    }
    let highest = exponent.checked_add(bits as i32 - 1)?;
    if highest > 1023 {
        return None;
    }
    if highest >= -1022 {
        let significand = (magnitude as u64) << (53 - bits);
        Some(f64::from_bits(
            sign | (((highest + 1023) as u64) << 52) | (significand & ((1u64 << 52) - 1)),
        ))
    } else {
        let shift = u32::try_from(exponent.checked_add(1074)?).ok()?;
        let significand = magnitude.checked_shl(shift)?;
        if significand >= 1u128 << 52 {
            return None;
        }
        Some(f64::from_bits(sign | significand as u64))
    }
}

/// For <=100 magnitude bits, nearest-even 53-bit head leaves <=47-bit residual.
/// The allowed endpoint 2^100 has101 bits but is exact with zero residual.
/// Both components are exact binary rationals; only Sum::value rounds their sum.
fn exact_sum(coefficient: i128, exponent: i32) -> Option<Sum> {
    if coefficient == 0 {
        return Some(Sum::default());
    }
    let magnitude = coefficient.unsigned_abs();
    if magnitude > COEFFICIENT_LIMIT {
        return None;
    }
    let bits = 128 - magnitude.leading_zeros();
    let shift = bits.saturating_sub(53);
    let mut head = magnitude >> shift;
    if shift > 0 {
        let remainder = magnitude & ((1u128 << shift) - 1);
        let halfway = 1u128 << (shift - 1);
        if remainder > halfway || (remainder == halfway && head & 1 == 1) {
            head += 1;
        }
    }
    let signed_head = if coefficient < 0 {
        -(head as i128)
    } else {
        head as i128
    };
    let aligned_head = signed_head.checked_shl(shift)?;
    let residual = coefficient.checked_sub(aligned_head)?;
    let high = scaled_integer(signed_head, exponent.checked_add(shift as i32)?)?;
    let low = scaled_integer(residual, exponent)?;
    Sum::from_parts([high, low]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_rational::BigRational;
    use num_traits::Zero;

    fn band(values: &[f64]) -> Band {
        Band {
            values: values.to_vec(),
            valid: vec![true; values.len()],
            unit: None,
        }
    }
    fn exact(value: f64) -> BigRational {
        BigRational::from_float(value).unwrap()
    }
    fn check_ranges(band: &Band, width: usize) {
        let summary = RowSummary::build(band, 0, width, &Options::default()).unwrap();
        assert_eq!(
            summary.bytes(),
            RowSummary::estimate_bytes(width, &Options::default()).unwrap()
        );
        for start in 0..=width {
            for end in start..=width {
                let selected: Vec<_> = (start..end)
                    .filter(|&i| band.valid[i])
                    .map(|i| band.values[i])
                    .collect();
                let expected: BigRational = selected.iter().map(|&x| exact(x)).sum();
                let (sum, count, min, max) = summary.range(start, end).unwrap();
                assert_eq!(
                    exact(sum.parts()[0]) + exact(sum.parts()[1]),
                    expected,
                    "{start}..{end}"
                );
                assert_eq!(count, selected.len());
                assert_eq!(min, selected.iter().fold(f64::INFINITY, |a, &b| a.min(b)));
                assert_eq!(
                    max,
                    selected.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b))
                );
            }
        }
    }
    #[test]
    fn large_prefix_tiny_interval_and_cancellation_are_exact() {
        let b = band(&[
            2f64.powi(80),
            1.,
            -2f64.powi(80),
            0.125,
            -0.125,
            3.,
            -2.,
            0.,
        ]);
        check_ranges(&b, b.values.len());
        let row = RowSummary::build(&b, 0, b.values.len(), &Options::default()).unwrap();
        assert_eq!(row.range(1, 2).unwrap().0.value(), 1.);
        assert_eq!(row.range(0, 3).unwrap().0.value(), 1.);
    }
    #[test]
    fn subnormals_and_masks_are_exact() {
        let tiny = f64::from_bits(1);
        let mut b = band(&[
            tiny,
            -tiny,
            f64::from_bits((1u64 << 52) - 1),
            tiny * 31.,
            f64::NAN,
            0.,
        ]);
        b.valid[4] = false;
        check_ranges(&b, b.values.len());
        b.valid[4] = true;
        assert!(RowSummary::build(&b, 0, b.values.len(), &Options::default()).is_none());
    }
    #[test]
    fn head_rounding_and_residual_match_rational_oracle() {
        let mut state = 0x3415aa39ef782cu64;
        for exponent in [-1074, -1000, -80, 0, 900] {
            for _ in 0..200 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let magnitude = ((state as u128) << 30)
                    | ((state.rotate_left(19) as u128) & ((1u128 << 30) - 1));
                let signed = if state & 1 == 0 {
                    magnitude as i128
                } else {
                    -(magnitude as i128)
                };
                let sum = exact_sum(signed, exponent).unwrap();
                let factor = if exponent >= 0 {
                    BigRational::from_integer(num_bigint::BigInt::from(1u8) << exponent as usize)
                } else {
                    BigRational::new(
                        1.into(),
                        num_bigint::BigInt::from(1u8) << (-exponent) as usize,
                    )
                };
                assert_eq!(
                    exact(sum.parts()[0]) + exact(sum.parts()[1]),
                    BigRational::from_integer(signed.into()) * factor
                );
            }
        }
        for coefficient in [
            (1i128 << 53) + 1,
            (1i128 << 53) + 3,
            (1i128 << 100) - 1,
            1i128 << 100,
        ] {
            let sum = exact_sum(coefficient, 0).unwrap();
            assert_eq!(
                exact(sum.parts()[0]) + exact(sum.parts()[1]),
                BigRational::from_integer(coefficient.into())
            );
        }
    }
    #[test]
    fn overflow_and_wide_exponents_fall_back_without_poisoning_other_ranges() {
        let b = band(&[f64::MAX, f64::MAX, -f64::MAX]);
        let row = RowSummary::build(&b, 0, 3, &Options::default()).unwrap();
        assert!(row.range(0, 2).is_none());
        assert_eq!(row.range(0, 3).unwrap().0.value(), f64::MAX);
        assert_eq!(row.range(1, 3).unwrap().0.value(), 0.);
        assert!(
            RowSummary::build(&band(&[2f64.powi(100), 1.]), 0, 2, &Options::default()).is_none()
        );
        assert!(
            RowSummary::build(
                &band(&[f64::MAX, f64::from_bits(1)]),
                0,
                2,
                &Options::default()
            )
            .is_none()
        );
    }
    #[test]
    fn requested_statistics_and_row_bounds_are_respected() {
        let b = band(&[1., 2., 4., 8., 16., 32.]);
        let opts = Options {
            statistics: Some(vec!["count".into()]),
            ..Options::default()
        };
        let row = RowSummary::build(&b, 1, 3, &opts).unwrap();
        let (sum, count, min, max) = row.range(0, 3).unwrap();
        assert!(exact(sum.value()).is_zero());
        assert_eq!(count, 3);
        assert_eq!(min, f64::INFINITY);
        assert_eq!(max, f64::NEG_INFINITY);
        assert!(row.range(2, 1).is_none());
        assert!(row.range(0, 4).is_none());
        assert!(RowSummary::build(&b, 2, 3, &opts).is_none());
        assert!(RowSummary::estimate_bytes(usize::MAX, &opts).is_none());
        assert!(RowSummary::estimate_bytes(MAX_ROW_BYTES, &opts).is_none());
        assert!(
            RowSummary::build(
                &b,
                0,
                3,
                &Options {
                    statistics: Some(vec!["variance".into()]),
                    ..Options::default()
                }
            )
            .is_none()
        );
        let wide = band(&[f64::MAX, f64::from_bits(1)]);
        assert!(RowSummary::build(&wide, 0, 2, &opts).is_some());
    }
}
