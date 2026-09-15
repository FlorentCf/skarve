//! Explicit central-moment and bounded exact-distribution reducers.
//! Geometry supplies unchanged positive native-cell fractions. These reducers
//! neither infer a mask nor reconstruct joint fields from marginal summaries.
use crate::{
    aggregate::Options,
    model::{Sum, check_cancel},
};
use anyhow::{Result, ensure};
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};
use serde::Serialize;
use std::sync::atomic::AtomicBool;

pub const DEFAULT_QUANTILE_SAMPLES: usize = 65_536;
pub const MAX_QUANTILE_SAMPLES: usize = 1_000_000;

#[derive(Clone, Copy, Default)]
struct Mean {
    hi: f64,
    lo: f64,
}
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}
impl Mean {
    fn add(self, rhs: Self) -> Self {
        let (s, e) = two_sum(self.hi, rhs.hi);
        let (hi, lo) = two_sum(s, e + self.lo + rhs.lo);
        Self { hi, lo }
    }
    fn sub(self, rhs: Self) -> Self {
        self.add(Self {
            hi: -rhs.hi,
            lo: -rhs.lo,
        })
    }
    fn scale(self, factor: f64) -> Self {
        let hi = self.hi * factor;
        let lo = self.hi.mul_add(factor, -hi) + self.lo * factor;
        let (hi, lo) = two_sum(hi, lo);
        Self { hi, lo }
    }
    fn value(self) -> f64 {
        self.hi + self.lo
    }
    fn finite(self) -> bool {
        self.hi.is_finite() && self.lo.is_finite()
    }
}

/// Rare exact central states are capped independently of the number of cells.
/// State operands have <=8192-bit numerators/denominators; the fixed central
/// merge expression has bounded intermediate products. No raw-square subtraction.
const MOMENT_INTEGER_BITS: u64 = 8192;
const MOMENT_TRANSIENT_BYTES: usize = 512 * 1024;
const MOMENT_RETAINED_BYTES: usize = 16 * 1024;
#[derive(Clone, Default)]
struct ExactMoments {
    weight: BigRational,
    mean: BigRational,
    m2: BigRational,
}
fn rational(x: f64) -> BigRational {
    BigRational::from_float(x).expect("finite central state")
}
fn rational_sum(s: Sum) -> BigRational {
    let p = s.parts();
    rational(p[0]) + rational(p[1])
}
impl ExactMoments {
    fn compact(self) -> Self {
        // Rebuild from the bounded normalized digits. This prevents retained
        // BigInts inheriting spare capacity from larger intermediate products.
        fn value(q: &BigRational) -> BigRational {
            let (ns, nb) = q.numer().to_bytes_le();
            let (ds, db) = q.denom().to_bytes_le();
            BigRational::new_raw(
                BigInt::from_bytes_le(ns, &nb),
                BigInt::from_bytes_le(ds, &db),
            )
        }
        Self {
            weight: value(&self.weight),
            mean: value(&self.mean),
            m2: value(&self.m2),
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.weight >= BigRational::zero() && self.m2 >= BigRational::zero(),
            "invalid exact central state"
        );
        for state in [&self.weight, &self.mean, &self.m2] {
            ensure!(
                state.numer().bits() <= MOMENT_INTEGER_BITS
                    && state.denom().bits() <= MOMENT_INTEGER_BITS,
                "exact central moment integer budget exceeded"
            );
        }
        Ok(())
    }
    fn merge(&mut self, rhs: &Self) -> Result<()> {
        if rhs.weight.is_zero() {
            return Ok(());
        }
        if self.weight.is_zero() {
            rhs.validate()?;
            *self = rhs.clone().compact();
            return Ok(());
        }
        let total = &self.weight + &rhs.weight;
        let delta = &rhs.mean - &self.mean;
        let mean = &self.mean + &delta * &rhs.weight / &total;
        let m2 = &self.m2 + &rhs.m2 + &delta * &delta * &self.weight * &rhs.weight / &total;
        let candidate = Self {
            weight: total,
            mean,
            m2,
        };
        candidate.validate()?;
        *self = candidate.compact();
        Ok(())
    }
    fn stddev(&self) -> Result<Option<f64>> {
        if self.weight.is_zero() {
            return Ok(None);
        }
        if self.m2.is_zero() {
            return Ok(Some(0.));
        }
        let q = &self.m2 / &self.weight;
        // Normalize the exact squared quantity by an even power of two.
        // Only the bounded mantissa is rounded before sqrt; neither a huge
        // variance nor a subnormal variance must be materialized as f64.
        let numerator = q.numer();
        let denominator = q.denom();
        let mut exponent = numerator.bits() as i64 - denominator.bits() as i64;
        let below = if exponent >= 0 {
            numerator < &(denominator << exponent as usize)
        } else {
            &(numerator << (-exponent) as usize) < denominator
        };
        if below {
            exponent -= 1;
        }
        let scale_exponent = exponent.div_euclid(2).clamp(-1074, 1023) as i32;
        let shift = 2 * scale_exponent;
        let normalized = if shift >= 0 {
            q / BigRational::from_integer(BigInt::from(1u8) << shift as usize)
        } else {
            q * BigRational::from_integer(BigInt::from(1u8) << (-shift) as usize)
        };
        let mantissa = normalized
            .to_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| anyhow::anyhow!("nonfinite population standard deviation"))?;
        let scale = if scale_exponent >= -1022 {
            f64::from_bits(((scale_exponent + 1023) as u64) << 52)
        } else {
            f64::from_bits(1u64 << (scale_exponent + 1074))
        };
        let value = mantissa.sqrt() * scale;
        ensure!(value.is_finite(), "nonfinite population standard deviation");
        ensure!(
            value > 0.,
            "positive standard deviation is below binary64 range"
        );
        Ok(Some(value))
    }
    fn variance(&self) -> Result<Option<f64>> {
        if self.weight.is_zero() {
            return Ok(None);
        }
        let exact = &self.m2 / &self.weight;
        let value = exact
            .to_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| anyhow::anyhow!("nonfinite population variance"))?;
        ensure!(
            exact.is_zero() || value > 0.,
            "positive variance is below binary64 range"
        );
        Ok(Some(value))
    }
}

/// Weights and central M2 use a common power-of-two scale. Scaling does not
/// change M2/W and prevents uniformly tiny positive coverage losing variance.
#[derive(Clone, Default)]
struct Moments {
    scale: f64,
    weight: Sum,
    mean: Mean,
    m2: Sum,
    exact: Option<ExactMoments>,
}
fn power_scale(x: f64) -> f64 {
    let bits = x.to_bits();
    let exponent = bits & (0x7ffu64 << 52);
    if exponent != 0 {
        f64::from_bits(exponent)
    } else {
        f64::from_bits(1u64 << (63 - bits.leading_zeros()))
    }
}
fn scaled_sum(s: Sum, factor: f64) -> Result<Sum> {
    let p = s.parts();
    let scaled = Sum::from_parts([p[0] * factor, p[1] * factor])?;
    ensure!(
        s.value() == 0. || scaled.value() > 0.,
        "positive moment state is below binary64 range after rescaling"
    );
    if scaled.value().is_subnormal() && factor != 1. {
        ensure!(
            rational_sum(s) * rational(factor) == rational_sum(scaled),
            "moment rescaling loses precision below binary64 normal range"
        );
    }
    Ok(scaled)
}
impl Moments {
    fn as_exact(&self) -> Result<ExactMoments> {
        if let Some(state) = &self.exact {
            return Ok(state.clone());
        }
        if self.scale == 0. {
            return Ok(ExactMoments::default());
        }
        let scale = rational(self.scale);
        let state = ExactMoments {
            weight: rational_sum(self.weight) * &scale,
            mean: rational(self.mean.hi) + rational(self.mean.lo),
            m2: rational_sum(self.m2) * scale,
        };
        state.validate()?;
        Ok(state)
    }
    fn merge_exact(&mut self, rhs: &ExactMoments) -> Result<()> {
        let mut state = self.as_exact()?;
        state.merge(rhs)?;
        self.exact = Some(state);
        Ok(())
    }
    fn sample_product(&mut self, value: f64, fraction: f64, weight: f64) -> Result<()> {
        if weight == 0. {
            return Ok(());
        }
        let effective = fraction * weight;
        if self.exact.is_some()
            || effective == 0.
            || !effective.is_finite()
            || (effective.is_subnormal()
                && rational(fraction) * rational(weight) != rational(effective))
        {
            return self.merge_exact(&ExactMoments {
                weight: rational(fraction) * rational(weight),
                mean: rational(value),
                m2: BigRational::zero(),
            });
        }
        self.sample(value, effective)
    }
    fn sample(&mut self, value: f64, weight: f64) -> Result<()> {
        ensure!(
            value.is_finite() && weight.is_finite() && weight >= 0.,
            "invalid moment input"
        );
        if weight == 0. {
            return Ok(());
        }
        if self.exact.is_some() {
            return self.merge_exact(&ExactMoments {
                weight: rational(weight),
                mean: rational(value),
                m2: BigRational::zero(),
            });
        }
        let scale = power_scale(weight);
        let mut state = Self {
            scale,
            mean: Mean { hi: value, lo: 0. },
            ..Self::default()
        };
        state.weight.add(weight / scale);
        self.merge(&state)
    }
    fn merge(&mut self, rhs: &Self) -> Result<()> {
        if self.exact.is_some() || rhs.exact.is_some() {
            return self.merge_exact(&rhs.as_exact()?);
        }
        // The floating implementation commits only after all guards pass.
        // A failed intermediate therefore leaves a convertible prior state.
        if self.merge_float(rhs).is_err() {
            self.merge_exact(&rhs.as_exact()?)?;
        }
        Ok(())
    }
    fn merge_float(&mut self, rhs: &Self) -> Result<()> {
        if rhs.scale == 0. {
            return Ok(());
        }
        if self.scale == 0. {
            *self = rhs.clone();
            return Ok(());
        }
        let scale = self.scale.max(rhs.scale);
        let mut wa = scaled_sum(self.weight, self.scale / scale)?;
        let wb = scaled_sum(rhs.weight, rhs.scale / scale)?;
        let a = wa.value();
        let b = wb.value();
        ensure!(
            a > 0. && b > 0.,
            "moment weight ratio is below binary64 range"
        );
        let mut m2 = scaled_sum(self.m2, self.scale / scale)?;
        m2.merge(scaled_sum(rhs.m2, rhs.scale / scale)?);
        wa.merge(wb);
        let total = wa.value();
        ensure!(total.is_finite() && total > 0., "nonfinite moment weight");
        let delta = rhs.mean.sub(self.mean);
        let d = delta.value();
        let ratio = b / total;
        ensure!(
            ratio > 0. && delta.finite(),
            "moment displacement or weight ratio exceeds binary64 range"
        );
        let mean = self.mean.add(delta.scale(ratio));
        let factor = if a < b {
            a * (b / total)
        } else {
            b * (a / total)
        };
        let cross_root = d * factor.sqrt();
        let cross = cross_root * cross_root;
        ensure!(
            mean.finite() && cross.is_finite(),
            "nonfinite central moment arithmetic"
        );
        ensure!(
            d == 0. || cross > 0.,
            "positive central moment is below binary64 range"
        );
        ensure!(
            !cross.is_subnormal(),
            "subnormal central term requires bounded rational arithmetic"
        );
        m2.add(cross);
        ensure!(
            m2.value().is_finite() && m2.value() >= 0.,
            "invalid central moment state"
        );
        *self = Self {
            scale,
            weight: wa,
            mean,
            m2,
            exact: None,
        };
        Ok(())
    }
    fn stddev(&self) -> Result<Option<f64>> {
        if let Some(state) = &self.exact {
            return state.stddev();
        }
        if self.scale == 0. {
            return Ok(None);
        }
        if self.m2.value() == 0. {
            return Ok(Some(0.));
        }
        let variance = self.m2.value() / self.weight.value();
        if variance.is_normal() {
            return Ok(Some(variance.sqrt()));
        }
        self.as_exact()?.stddev()
    }
    fn variance(&self) -> Result<Option<f64>> {
        if let Some(state) = &self.exact {
            return state.variance();
        }
        if self.scale == 0. {
            return Ok(None);
        }
        let v = self.m2.value() / self.weight.value();
        ensure!(v.is_finite() && v >= 0., "nonfinite population variance");
        ensure!(
            self.m2.value() == 0. || v > 0.,
            "positive variance is below binary64 range"
        );
        Ok(Some(v))
    }
}

/// Exact integer mass in units of 2^-1074 for binary64 coverage fractions.
/// Unlike comparing rounded totals, this preserves arbitrarily close ties.
#[derive(Clone, Default)]
struct CoverageMass(BigUint);
impl CoverageMass {
    fn add(&mut self, f: f64) -> Result<()> {
        ensure!(
            f.is_finite() && f > 0. && f <= 1.,
            "invalid categorical coverage"
        );
        let bits = f.to_bits();
        let exponent = (bits >> 52) as usize;
        let fraction = bits & ((1u64 << 52) - 1);
        let term = if exponent == 0 {
            BigUint::from(fraction)
        } else {
            BigUint::from(fraction | (1u64 << 52)) << (exponent - 1)
        };
        self.0 += term;
        ensure!(self.0.bits() <= 1152, "categorical support budget exceeded");
        Ok(())
    }
    fn value(&self) -> Result<f64> {
        BigRational::new(BigInt::from(self.0.clone()), BigInt::from(1u8) << 1074)
            .to_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| anyhow::anyhow!("nonfinite categorical support"))
    }
}

#[derive(Clone)]
struct Categories {
    domain: Vec<f64>,
    support: Vec<CoverageMass>,
}
#[derive(Serialize)]
pub struct CategoryResult {
    pub values: Vec<f64>,
    pub covered_cell_equivalents: Vec<f64>,
    pub fractions: Vec<Option<f64>>,
    pub tie_rule: &'static str,
}
impl Categories {
    fn add(&mut self, value: f64, f: f64) -> Result<()> {
        let value = if value == 0. { 0. } else { value };
        let index = self
            .domain
            .binary_search_by(|v| v.total_cmp(&value))
            .map_err(|_| anyhow::anyhow!("valid value {value} is outside category_values"))?;
        self.support[index].add(f)
    }
    fn merge(&mut self, rhs: &Self) -> Result<()> {
        ensure!(self.domain == rhs.domain, "categorical domains differ");
        for (a, b) in self.support.iter_mut().zip(&rhs.support) {
            a.0 += &b.0;
            ensure!(a.0.bits() <= 1152, "categorical support budget exceeded");
        }
        Ok(())
    }
    fn finish(self) -> Result<(CategoryResult, Option<f64>, usize)> {
        let total: BigUint = self.support.iter().map(|s| &s.0).sum();
        let mut majority = None;
        let mut best = BigUint::zero();
        let mut variety = 0;
        let mut counts = Vec::with_capacity(self.domain.len());
        let mut fractions = Vec::with_capacity(self.domain.len());
        for (&value, support) in self.domain.iter().zip(&self.support) {
            if !support.0.is_zero() {
                variety += 1;
                if support.0 > best {
                    best = support.0.clone();
                    majority = Some(value);
                }
            }
            counts.push(support.value()?);
            fractions.push(if total.is_zero() {
                None
            } else {
                Some(
                    BigRational::new(BigInt::from(support.0.clone()), BigInt::from(total.clone()))
                        .to_f64()
                        .ok_or_else(|| anyhow::anyhow!("categorical fraction conversion failed"))?,
                )
            });
        }
        Ok((
            CategoryResult {
                values: self.domain,
                covered_cell_equivalents: counts,
                fractions,
                tie_rule: "smallest_numeric_value",
            },
            majority,
            variety,
        ))
    }
}

#[derive(Clone, Copy)]
struct Sample {
    value: f64,
    coverage: f64,
    weight: f64,
}
#[derive(Serialize)]
pub struct QuantileResult {
    pub probabilities: Vec<f64>,
    pub values: Vec<Option<f64>>,
    pub method: &'static str,
    pub weighting: &'static str,
    pub exact_rank: bool,
}

#[derive(Serialize)]
pub struct MomentDiagnostics {
    pub arithmetic: &'static str,
    pub unweighted_rational_fallback: bool,
    pub weighted_rational_fallback: bool,
    pub max_retained_integer_bits: u64,
}
#[derive(Default, Serialize)]
pub struct ReducerResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moment_diagnostics: Option<MomentDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variance: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stddev: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_variance: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_stddev: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<CategoryResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub majority: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variety: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantiles: Option<QuantileResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_median: Option<Option<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_quantiles: Option<QuantileResult>,
}

pub(crate) struct Reducers {
    options: Options,
    moments: Option<Moments>,
    weighted_moments: Option<Moments>,
    categories: Option<Categories>,
    samples: Vec<Sample>,
    error: Option<String>,
}
impl Reducers {
    pub(crate) fn retained_bytes(options: &Options) -> Result<usize> {
        let categories = options.category_values.as_ref().map_or(0, Vec::len);
        let samples = if options.needs_quantiles() {
            options.quantile_limit()
        } else {
            0
        };
        let moment_count =
            usize::from(options.wants_new("variance") || options.wants_new("stddev"))
                + usize::from(
                    options.wants_new("weighted_variance") || options.wants_new("weighted_stddev"),
                );
        categories
            .checked_mul(512)
            .and_then(|n| n.checked_add(moment_count * MOMENT_RETAINED_BYTES))
            .and_then(|n| {
                samples
                    .checked_mul(std::mem::size_of::<Sample>())
                    .and_then(|q| n.checked_add(q))
            })
            .and_then(|n| n.checked_add(4096))
            .ok_or_else(|| anyhow::anyhow!("reducer memory estimate overflow"))
    }
    pub(crate) fn transient_bytes(options: &Options) -> usize {
        let moments = options.wants_new("variance")
            || options.wants_new("stddev")
            || options.wants_new("weighted_variance")
            || options.wants_new("weighted_stddev");
        4096 + if moments { MOMENT_TRANSIENT_BYTES } else { 0 }
            + if options.needs_quantiles() {
                128 * 1024
            } else {
                0
            }
    }
    pub(crate) fn new(options: &Options) -> Self {
        Self {
            options: options.clone(),
            moments: (options.wants_new("variance") || options.wants_new("stddev"))
                .then(Moments::default),
            weighted_moments: (options.wants_new("weighted_variance")
                || options.wants_new("weighted_stddev"))
            .then(Moments::default),
            categories: options.category_values.as_ref().map(|values| Categories {
                domain: values
                    .iter()
                    .map(|&x| if x == 0. { 0. } else { x })
                    .collect(),
                support: vec![CoverageMass::default(); values.len()],
            }),
            samples: vec![],
            error: None,
        }
    }
    pub(crate) fn check_error(&self) -> Result<()> {
        if let Some(error) = &self.error {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
    pub(crate) fn cell(&mut self, value: f64, fraction: f64, weight: Option<f64>) {
        if self.error.is_none() {
            if let Err(error) = self.try_cell(value, fraction, weight) {
                self.error = Some(error.to_string());
            }
        }
    }
    fn try_cell(&mut self, value: f64, fraction: f64, weight: Option<f64>) -> Result<()> {
        ensure!(
            value.is_finite() && fraction.is_finite() && fraction > 0. && fraction <= 1.,
            "invalid reducer contribution"
        );
        if let Some(m) = &mut self.moments {
            m.sample(value, fraction)?;
        }
        if let Some(c) = &mut self.categories {
            c.add(value, fraction)?;
        }
        let weight = weight.filter(|w| w.is_finite() && *w >= 0.);
        if let (Some(m), Some(w)) = (&mut self.weighted_moments, weight) {
            m.sample_product(value, fraction, w)?;
        }
        if self.options.needs_quantiles()
            && (self.options.needs_unweighted_quantiles() || weight.is_some_and(|w| w > 0.))
        {
            let limit = self.options.quantile_limit();
            ensure!(
                self.samples.len() < limit,
                "exact quantile sample budget exceeded ({limit})"
            );
            if self.samples.len() == self.samples.capacity() {
                self.samples
                    .try_reserve_exact((limit - self.samples.len()).min(4096))?;
            }
            self.samples.push(Sample {
                value: if value == 0. { 0. } else { value },
                coverage: fraction,
                weight: weight.unwrap_or(-1.),
            });
        }
        Ok(())
    }
    pub(crate) fn merge(&mut self, rhs: Self) -> Result<()> {
        if let Some(error) = self.error.as_ref().or(rhs.error.as_ref()) {
            anyhow::bail!("{error}");
        }
        ensure!(
            self.options.reducer_identity() == rhs.options.reducer_identity(),
            "reducer configurations differ"
        );
        if let (Some(a), Some(b)) = (&mut self.moments, rhs.moments) {
            a.merge(&b)?;
        }
        if let (Some(a), Some(b)) = (&mut self.weighted_moments, rhs.weighted_moments) {
            a.merge(&b)?;
        }
        if let (Some(a), Some(b)) = (&mut self.categories, rhs.categories) {
            a.merge(&b)?;
        }
        if !rhs.samples.is_empty() {
            let total = self
                .samples
                .len()
                .checked_add(rhs.samples.len())
                .ok_or_else(|| anyhow::anyhow!("quantile sample count overflow"))?;
            ensure!(
                total <= self.options.quantile_limit(),
                "exact quantile sample budget exceeded on merge"
            );
            self.samples.try_reserve_exact(rhs.samples.len())?;
            self.samples.extend(rhs.samples);
        }
        Ok(())
    }
    fn quantiles(
        &self,
        probabilities: &[f64],
        weighted: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<Option<f64>>> {
        let mass = |s: &Sample| -> Option<BigRational> {
            if weighted && s.weight <= 0. {
                return None;
            }
            let f = BigRational::from_float(s.coverage)?;
            Some(if weighted {
                f * BigRational::from_float(s.weight)?
            } else {
                f
            })
        };
        let mut total = BigRational::zero();
        for (i, sample) in self.samples.iter().enumerate() {
            if i % 1024 == 0 {
                check_cancel(cancel)?;
            }
            if let Some(weight) = mass(sample) {
                total += weight;
            }
        }
        if total.is_zero() {
            return Ok(vec![None; probabilities.len()]);
        }
        let thresholds: Vec<_> = probabilities
            .iter()
            .map(|&p| BigRational::from_float(p).unwrap() * &total)
            .collect();
        let mut results = vec![None; probabilities.len()];
        let mut cumulative = BigRational::zero();
        for (n, sample) in self.samples.iter().enumerate() {
            if n % 1024 == 0 {
                check_cancel(cancel)?;
            }
            let Some(weight) = mass(sample) else {
                continue;
            };
            cumulative += weight;
            for (i, threshold) in thresholds.iter().enumerate() {
                if results[i].is_none() && cumulative >= *threshold {
                    results[i] = Some(sample.value);
                }
            }
        }
        ensure!(
            results.iter().all(Option::is_some),
            "exact quantile rank not found"
        );
        Ok(results)
    }
    pub(crate) fn finish(mut self, cancel: &AtomicBool) -> Result<ReducerResult> {
        check_cancel(cancel)?;
        if let Some(error) = &self.error {
            anyhow::bail!("{error}");
        }
        let mut out = ReducerResult::default();
        if self.moments.is_some() || self.weighted_moments.is_some() {
            out.moment_diagnostics = Some(MomentDiagnostics {
                arithmetic: "compensated_f64_with_bounded_rational_fallback",
                unweighted_rational_fallback: self
                    .moments
                    .as_ref()
                    .is_some_and(|m| m.exact.is_some()),
                weighted_rational_fallback: self
                    .weighted_moments
                    .as_ref()
                    .is_some_and(|m| m.exact.is_some()),
                max_retained_integer_bits: MOMENT_INTEGER_BITS,
            });
        }
        if let Some(m) = &self.moments {
            if self.options.wants_new("variance") {
                out.variance = Some(m.variance()?);
            }
            if self.options.wants_new("stddev") {
                out.stddev = Some(m.stddev()?);
            }
        }
        if let Some(m) = &self.weighted_moments {
            if self.options.wants_new("weighted_variance") {
                out.weighted_variance = Some(m.variance()?);
            }
            if self.options.wants_new("weighted_stddev") {
                out.weighted_stddev = Some(m.stddev()?);
            }
        }
        if let Some(c) = self.categories.take() {
            let (categories, majority, variety) = c.finish()?;
            if self.options.wants_new("categories") {
                out.categories = Some(categories);
            }
            if self.options.wants_new("majority") {
                out.majority = Some(majority);
            }
            if self.options.wants_new("variety") {
                out.variety = Some(variety);
            }
        }
        if self.options.needs_quantiles() {
            self.samples
                .sort_unstable_by(|a, b| a.value.total_cmp(&b.value));
            check_cancel(cancel)?;
            for weighted in [false, true] {
                let (median_name, quantile_name) = if weighted {
                    ("weighted_median", "weighted_quantiles")
                } else {
                    ("median", "quantiles")
                };
                // Reuse only an existing public target, independently per mode.
                // Missing 0.5 retains the original separate-call path below.
                let shared_median_index = if self.options.wants_new(median_name)
                    && self.options.wants_new(quantile_name)
                {
                    self.options
                        .quantiles
                        .as_ref()
                        .expect("validated quantiles")
                        .iter()
                        .position(|&p| p == 0.5)
                } else {
                    None
                };
                if self.options.wants_new(median_name) && shared_median_index.is_none() {
                    let v = self.quantiles(&[0.5], weighted, cancel)?[0];
                    if weighted {
                        out.weighted_median = Some(v);
                    } else {
                        out.median = Some(v);
                    }
                }
                if self.options.wants_new(quantile_name) {
                    let probabilities = self
                        .options
                        .quantiles
                        .as_ref()
                        .expect("validated quantiles");
                    let result = QuantileResult {
                        probabilities: probabilities.clone(),
                        values: self.quantiles(probabilities, weighted, cancel)?,
                        method: "inverted_cdf",
                        weighting: if weighted {
                            "coverage_times_weight"
                        } else {
                            "coverage"
                        },
                        exact_rank: true,
                    };
                    if let Some(index) = shared_median_index {
                        check_cancel(cancel)?;
                        let median = result.values[index];
                        if weighted {
                            out.weighted_median = Some(median);
                        } else {
                            out.median = Some(median);
                        }
                    }
                    if weighted {
                        out.weighted_quantiles = Some(result);
                    } else {
                        out.quantiles = Some(result);
                    }
                }
            }
        }
        Ok(out)
    }
}
