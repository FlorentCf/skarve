use crate::{coverage::Plan, model::*};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, atomic::AtomicBool};

const BLOCK: usize = 64;
#[derive(Clone, Debug)]
struct Block {
    sum: Sum,
    count: usize,
    min: f64,
    max: f64,
}
#[derive(Debug)]
pub struct Prepared {
    pub source_id: String,
    pub grid: Grid,
    blocks: Vec<Vec<Block>>,
    pub bytes: usize,
}
impl Prepared {
    pub fn estimate_bytes(r: &Raster) -> Result<usize> {
        r.grid
            .width
            .div_ceil(BLOCK)
            .checked_mul(r.grid.height)
            .and_then(|n| n.checked_mul(r.bands.len()))
            .and_then(|n| n.checked_mul(std::mem::size_of::<Block>()))
            .ok_or_else(|| anyhow::anyhow!("index size overflow"))
    }
    pub fn build(r: &Raster, cancel: &AtomicBool) -> Result<Self> {
        r.validate()?;
        let per_row = r.grid.width.div_ceil(BLOCK);
        let n = per_row * r.grid.height;
        let bytes = Self::estimate_bytes(r)?;
        ensure!(
            r.bytes() + bytes <= MAX_BYTES,
            "index memory budget exceeded"
        );
        let mut blocks = Vec::new();
        for band in &r.bands {
            let mut bb = Vec::with_capacity(n);
            for row in 0..r.grid.height {
                check_cancel(cancel)?;
                for x in (0..r.grid.width).step_by(BLOCK) {
                    let mut b = Block {
                        sum: Sum::default(),
                        count: 0,
                        min: f64::INFINITY,
                        max: f64::NEG_INFINITY,
                    };
                    for col in x..(x + BLOCK).min(r.grid.width) {
                        let i = row * r.grid.width + col;
                        if band.valid[i] {
                            let v = band.values[i];
                            b.sum.add(v);
                            b.count += 1;
                            b.min = b.min.min(v);
                            b.max = b.max.max(v);
                        }
                    }
                    ensure!(b.sum.value().is_finite(), "nonfinite index arithmetic");
                    bb.push(b);
                }
            }
            blocks.push(bb);
        }
        Ok(Self {
            source_id: r.source_id.clone(),
            grid: r.grid.clone(),
            blocks,
            bytes,
        })
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub bands: Vec<usize>,
    pub histogram_edges: Option<Vec<f64>>,
    pub weight_band: Option<usize>,
    pub statistics: Option<Vec<String>>,
    pub category_values: Option<Vec<f64>>,
    pub quantiles: Option<Vec<f64>>,
    pub quantile_max_samples: Option<usize>,
}
impl Options {
    /// Extensions are opt-in; omission keeps the legacy result and work.
    pub fn wants_new(&self, name: &str) -> bool {
        self.statistics
            .as_ref()
            .is_some_and(|s| s.iter().any(|v| v == name))
    }
    pub fn has_new_reducers(&self) -> bool {
        [
            "variance",
            "stddev",
            "weighted_variance",
            "weighted_stddev",
            "categories",
            "majority",
            "variety",
            "median",
            "quantiles",
            "weighted_median",
            "weighted_quantiles",
        ]
        .iter()
        .any(|s| self.wants_new(s))
    }
    pub fn summaries_eligible(&self) -> bool {
        self.histogram_edges.is_none() && self.weight_band.is_none() && !self.has_new_reducers()
    }
    pub fn needs_unweighted_quantiles(&self) -> bool {
        self.wants_new("median") || self.wants_new("quantiles")
    }
    pub fn needs_quantiles(&self) -> bool {
        self.needs_unweighted_quantiles()
            || self.wants_new("weighted_median")
            || self.wants_new("weighted_quantiles")
    }
    pub fn quantile_limit(&self) -> usize {
        self.quantile_max_samples
            .unwrap_or(crate::reducers::DEFAULT_QUANTILE_SAMPLES)
    }
    pub fn reducer_identity(&self) -> String {
        serde_json::json!({"histogram_edges":self.histogram_edges,"weight_band":self.weight_band,"statistics":self.statistics,"category_values":self.category_values,"quantiles":self.quantiles,"quantile_max_samples":self.quantile_max_samples}).to_string()
    }
    pub fn wants(&self, name: &str) -> bool {
        self.statistics
            .as_ref()
            .is_none_or(|s| s.iter().any(|v| v == name))
    }
    pub fn needs_sum(&self) -> bool {
        self.wants("sum") || self.wants("mean")
    }
    pub fn needs_min(&self) -> bool {
        self.wants("min")
    }
    pub fn needs_max(&self) -> bool {
        self.wants("max")
    }
    pub fn needs_weighted_sum(&self) -> bool {
        self.wants("weighted_sum") || self.wants("weighted_mean")
    }
    pub fn needs_weight_sum(&self) -> bool {
        self.wants("weight_sum") || self.wants("weighted_mean")
    }
    pub fn validate(&self, band_count: usize) -> Result<()> {
        ensure!(
            self.bands.len() <= 20 && self.bands.iter().all(|&b| b < band_count),
            "band index out of range"
        );
        ensure!(
            self.bands
                .iter()
                .enumerate()
                .all(|(i, b)| !self.bands[..i].contains(b)),
            "duplicate bands not supported"
        );
        if let Some(w) = self.weight_band {
            ensure!(w < band_count, "weight band out of range");
        }
        if let Some(edges) = &self.histogram_edges {
            ensure!(
                (2..=257).contains(&edges.len())
                    && edges.iter().all(|v| v.is_finite())
                    && edges.windows(2).all(|w| w[0] < w[1]),
                "histogram edges must be finite strictly increasing, 2..257 entries"
            );
        }
        if let Some(stats) = &self.statistics {
            ensure!(
                !stats.is_empty() && stats.len() <= 21,
                "statistics must contain 1..21 names"
            );
            for (i, s) in stats.iter().enumerate() {
                ensure!(
                    [
                        "sum",
                        "support",
                        "mean",
                        "min",
                        "max",
                        "count",
                        "histogram",
                        "weighted_sum",
                        "weight_sum",
                        "weighted_mean",
                        "variance",
                        "stddev",
                        "weighted_variance",
                        "weighted_stddev",
                        "categories",
                        "majority",
                        "variety",
                        "median",
                        "quantiles",
                        "weighted_median",
                        "weighted_quantiles"
                    ]
                    .contains(&s.as_str()),
                    "unsupported statistic {s}"
                );
                ensure!(!stats[..i].contains(s), "duplicate statistic");
            }
            ensure!(
                !self.wants("histogram") || self.histogram_edges.is_some(),
                "histogram requires edges"
            );
            ensure!(
                !stats.iter().any(|s| s.starts_with("weight")) || self.weight_band.is_some(),
                "weighted statistics require weight_band"
            );
            ensure!(
                self.histogram_edges.is_none() || self.wants("histogram"),
                "histogram_edges supplied without histogram statistic"
            );
            ensure!(
                self.weight_band.is_none() || stats.iter().any(|s| s.starts_with("weight")),
                "weight_band supplied without weighted statistic"
            );
        }
        let categories = ["categories", "majority", "variety"]
            .iter()
            .any(|s| self.wants_new(s));
        ensure!(
            categories == self.category_values.is_some(),
            "categorical statistics require category_values and unused domains are rejected"
        );
        if let Some(values) = &self.category_values {
            ensure!(
                (1..=256).contains(&values.len())
                    && values.iter().all(|v| v.is_finite())
                    && values.windows(2).all(|w| w[0] < w[1]),
                "category_values must be finite strictly increasing, 1..256 entries"
            );
        }
        let quantiles = self.wants_new("quantiles") || self.wants_new("weighted_quantiles");
        ensure!(
            quantiles == self.quantiles.is_some(),
            "quantiles statistic requires probabilities and unused probabilities are rejected"
        );
        if let Some(probabilities) = &self.quantiles {
            ensure!(
                (1..=32).contains(&probabilities.len())
                    && probabilities
                        .iter()
                        .all(|p| p.is_finite() && (0. ..=1.).contains(p))
                    && probabilities.windows(2).all(|w| w[0] < w[1]),
                "quantiles must be finite strictly increasing probabilities in [0,1], 1..32 entries"
            );
        }
        if let Some(limit) = self.quantile_max_samples {
            ensure!(
                self.needs_quantiles()
                    && (1..=crate::reducers::MAX_QUANTILE_SAMPLES).contains(&limit),
                "quantile_max_samples requires a quantile/median statistic and a limit in 1..1000000"
            );
        }
        Ok(())
    }
    pub fn selected_bands(&self, count: usize) -> Vec<usize> {
        if self.bands.is_empty() {
            (0..count).collect()
        } else {
            self.bands.clone()
        }
    }
    /// Explicit requests omit values that were never reduced. Coverage quality
    /// fields remain available and account for support/count dependencies.
    pub fn project(&self, result: &mut serde_json::Value) {
        if self.statistics.is_none() {
            return;
        }
        if let Some(bands) = result.get_mut("bands").and_then(|v| v.as_array_mut()) {
            for band in bands {
                if let Some(obj) = band.as_object_mut() {
                    for (name, key) in [
                        ("sum", "fractional_sum"),
                        ("mean", "coverage_weighted_mean"),
                        ("min", "min"),
                        ("max", "max"),
                        ("histogram", "histogram"),
                        ("weighted_sum", "weighted_sum"),
                        ("weight_sum", "weight_sum"),
                        ("weighted_mean", "weighted_mean"),
                    ] {
                        if !self.wants(name) {
                            obj.remove(key);
                        }
                    }
                }
            }
        }
        result["statistics"] = serde_json::json!(self.statistics);
        result["reduction_dependencies"] = serde_json::json!({"sum":self.needs_sum(),"support":true,"count":true,"min":self.needs_min(),"max":self.needs_max(),"histogram":self.histogram_edges.is_some(),"joint_weighted_fields":self.weight_band.is_some(),"central_moments":self.wants_new("variance")||self.wants_new("stddev")||self.wants_new("weighted_variance")||self.wants_new("weighted_stddev"),"exact_categories":self.category_values.is_some(),"exact_quantiles":self.needs_quantiles()});
    }
}
#[derive(Serialize)]
pub struct Histogram {
    pub edges: Vec<f64>,
    #[serde(rename = "counts")]
    pub covered_cell_equivalents: Vec<f64>,
    pub underflow: f64,
    pub overflow: f64,
}
#[derive(Serialize)]
pub struct BandResult {
    pub band: usize,
    pub fractional_sum: f64,
    pub covered_cell_equivalents: f64,
    pub selected_cell_equivalents: f64,
    pub missing_cell_equivalents: f64,
    pub outside_cell_equivalents: f64,
    pub intersecting_cell_count: usize,
    pub valid_cell_count: usize,
    pub coverage_weighted_mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub status: String,
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_sum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_sum: Option<f64>,
    pub weighted_mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_valid_cell_equivalents: Option<f64>,
    #[serde(flatten)]
    pub extensions: crate::reducers::ReducerResult,
}
/// Shared numeric batch presentation; strict accumulation is unchanged.
#[derive(Serialize)]
pub(crate) struct NumericBand {
    pub band: usize,
    pub fractional_sum: f64,
    pub covered_cell_equivalents: f64,
    pub valid_cell_count: usize,
    pub coverage_weighted_mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

pub(crate) struct Acc {
    sum: Sum,
    valid: Sum,
    count: usize,
    min: f64,
    max: f64,
    hist: Vec<Sum>,
    under: Sum,
    over: Sum,
    weighted: Sum,
    weights: Sum,
    weighted_valid: Sum,
    reduce_sum: bool,
    reduce_min: bool,
    reduce_max: bool,
    reduce_weighted: bool,
    reduce_weights: bool,
    extensions: Option<crate::reducers::Reducers>,
    merge_key: Arc<str>,
}
impl Acc {
    /// Retained state for one output band and polygon, including rare states.
    pub(crate) fn retained_bytes(options: &Options) -> Result<usize> {
        let bins = options
            .histogram_edges
            .as_ref()
            .map_or(0, |e| e.len().saturating_sub(1));
        let extension = if options.has_new_reducers() {
            crate::reducers::Reducers::retained_bytes(options)?
        } else {
            0
        };
        std::mem::size_of::<Self>()
            .checked_add(bins * std::mem::size_of::<Sum>())
            .and_then(|n| n.checked_add(extension))
            .and_then(|n| n.checked_add(options.reducer_identity().len() + 4096))
            .ok_or_else(|| anyhow::anyhow!("accumulator memory estimate overflow"))
    }
    /// Reusable scratch for one active reducer operation in a single worker.
    pub(crate) fn transient_bytes(options: &Options) -> usize {
        if options.has_new_reducers() {
            crate::reducers::Reducers::transient_bytes(options)
        } else {
            4096
        }
    }
    #[cfg(test)]
    pub(crate) fn estimate_bytes(options: &Options) -> Result<usize> {
        Self::retained_bytes(options)?
            .checked_add(Self::transient_bytes(options))
            .ok_or_else(|| anyhow::anyhow!("accumulator memory estimate overflow"))
    }
    pub(crate) fn new(bins: usize, options: &Options) -> Self {
        Self::new_shared(bins, options, Arc::from(options.reducer_identity()))
    }
    /// Batch callers serialize the immutable reducer identity once per slice.
    /// Pointer sharing changes allocation only; merge still compares contents.
    pub(crate) fn new_shared(bins: usize, options: &Options, merge_key: Arc<str>) -> Self {
        Self {
            sum: Sum::default(),
            valid: Sum::default(),
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            hist: vec![Sum::default(); bins],
            under: Sum::default(),
            over: Sum::default(),
            weighted: Sum::default(),
            weights: Sum::default(),
            weighted_valid: Sum::default(),
            reduce_sum: options.needs_sum(),
            reduce_min: options.needs_min(),
            reduce_max: options.needs_max(),
            reduce_weighted: options.needs_weighted_sum(),
            reduce_weights: options.needs_weight_sum(),
            extensions: options
                .has_new_reducers()
                .then(|| crate::reducers::Reducers::new(options)),
            merge_key,
        }
    }
    /// Finish the existing six-field numeric lane without constructing and
    /// discarding rich status/unit/extension output. Same finite checks as full.
    pub(crate) fn finish_numeric(self, bi: usize, cancel: &AtomicBool) -> Result<NumericBand> {
        check_cancel(cancel)?;
        ensure!(
            self.hist.is_empty()
                && self.extensions.is_none()
                && self.reduce_sum
                && self.reduce_min
                && self.reduce_max
                && !self.reduce_weighted
                && !self.reduce_weights,
            "numeric output requires the six-statistic accumulator"
        );
        let sum = self.sum.value();
        let valid = self.valid.value();
        let ws = self.weighted.value();
        let ww = self.weights.value();
        ensure!(
            [sum, valid, ws, ww].iter().all(|v| v.is_finite()),
            "nonfinite aggregation arithmetic"
        );
        ensure!(
            (valid == 0. || (sum / valid).is_finite()) && (ww == 0. || (ws / ww).is_finite()),
            "nonfinite mean arithmetic"
        );
        Ok(NumericBand {
            band: bi,
            fractional_sum: sum,
            covered_cell_equivalents: valid,
            valid_cell_count: self.count,
            coverage_weighted_mean: (valid > 0.).then(|| sum / valid),
            min: (self.count > 0).then_some(self.min),
            max: (self.count > 0).then_some(self.max),
        })
    }
    /// Surface a stored reducer failure before requesting the next source tile.
    pub(crate) fn check_error(&self) -> Result<()> {
        self.extensions.as_ref().map_or(Ok(()), |r| r.check_error())
    }
    pub(crate) fn cell(
        &mut self,
        band: &Band,
        i: usize,
        f: f64,
        weights: Option<&Band>,
        edges: Option<&[f64]>,
    ) {
        if !band.valid[i] {
            return;
        }
        let v = band.values[i];
        if let Some(reducers) = &mut self.extensions {
            reducers.cell(
                v,
                f,
                weights.and_then(|w| (w.valid[i] && w.values[i] >= 0.).then_some(w.values[i])),
            );
        }
        if self.reduce_sum {
            self.sum.add(v * f);
        }
        self.valid.add(f);
        self.count += 1;
        if self.reduce_min {
            self.min = self.min.min(v);
        }
        if self.reduce_max {
            self.max = self.max.max(v);
        }
        if let Some(w) = weights {
            if w.valid[i] && w.values[i] >= 0. {
                let weight = w.values[i];
                if self.reduce_weighted {
                    self.weighted.add(v * weight * f);
                }
                if self.reduce_weights {
                    self.weights.add(weight * f);
                }
                self.weighted_valid.add(f);
            }
        }
        if let Some(e) = edges {
            if v < e[0] {
                self.under.add(f)
            } else if v > *e.last().unwrap() {
                self.over.add(f)
            } else {
                let b = e
                    .partition_point(|edge| *edge <= v)
                    .saturating_sub(1)
                    .min(e.len() - 2);
                self.hist[b].add(f);
            }
        }
    }
    fn block(&mut self, b: &Block) {
        if self.reduce_sum {
            self.sum.merge(b.sum);
        }
        self.valid.add(b.count as f64);
        self.count += b.count;
        if self.reduce_min {
            self.min = self.min.min(b.min);
        }
        if self.reduce_max {
            self.max = self.max.max(b.max);
        }
    }
    pub(crate) fn merge_summary(&mut self, sum: Sum, count: usize, min: f64, max: f64) {
        self.block(&Block {
            sum,
            count,
            min,
            max,
        });
    }
    /// Merge disjoint partial contributions, never rounded serialized answers.
    /// The job owns source/geometry identity and exactly-once task accounting.
    pub(crate) fn merge_acc(&mut self, rhs: Self) -> Result<()> {
        ensure!(
            self.merge_key == rhs.merge_key && self.hist.len() == rhs.hist.len(),
            "accumulator configurations differ"
        );
        match (&mut self.extensions, rhs.extensions) {
            (Some(a), Some(b)) => a.merge(b)?,
            (None, None) => (),
            _ => anyhow::bail!("accumulator capabilities differ"),
        }
        self.sum.merge(rhs.sum);
        self.valid.merge(rhs.valid);
        self.count = self
            .count
            .checked_add(rhs.count)
            .ok_or_else(|| anyhow::anyhow!("positive cell count overflow"))?;
        self.min = self.min.min(rhs.min);
        self.max = self.max.max(rhs.max);
        for (a, b) in self.hist.iter_mut().zip(rhs.hist) {
            a.merge(b);
        }
        self.under.merge(rhs.under);
        self.over.merge(rhs.over);
        self.weighted.merge(rhs.weighted);
        self.weights.merge(rhs.weights);
        self.weighted_valid.merge(rhs.weighted_valid);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn finish(
        self,
        bi: usize,
        selected: f64,
        area: f64,
        intersecting: usize,
        unit: Option<String>,
        options: &Options,
    ) -> Result<BandResult> {
        self.finish_cancellable(
            bi,
            selected,
            area,
            intersecting,
            unit,
            options,
            &AtomicBool::new(false),
        )
    }
    pub(crate) fn finish_cancellable(
        self,
        bi: usize,
        selected: f64,
        area: f64,
        intersecting: usize,
        unit: Option<String>,
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<BandResult> {
        check_cancel(cancel)?;
        let sum = self.sum.value();
        let valid = self.valid.value();
        let ws = self.weighted.value();
        let ww = self.weights.value();
        ensure!(
            [sum, valid, ws, ww].iter().all(|v| v.is_finite()),
            "nonfinite aggregation arithmetic"
        );
        ensure!(
            (valid == 0. || (sum / valid).is_finite()) && (ww == 0. || (ws / ww).is_finite()),
            "nonfinite mean arithmetic"
        );
        let missing = (selected - valid).max(0.);
        let outside = (area - selected).max(0.);
        let status = if area == 0. {
            "empty"
        } else if selected == 0. {
            "outside"
        } else if valid == 0. {
            "no_valid_data"
        } else if missing > 1e-9 || outside > 1e-9 {
            "partial"
        } else {
            "ok"
        };
        Ok(BandResult {
            band: bi,
            fractional_sum: sum,
            covered_cell_equivalents: valid,
            selected_cell_equivalents: selected,
            missing_cell_equivalents: missing,
            outside_cell_equivalents: outside,
            intersecting_cell_count: intersecting,
            valid_cell_count: self.count,
            coverage_weighted_mean: if valid > 0. { Some(sum / valid) } else { None },
            min: if self.count > 0 && options.needs_min() {
                Some(self.min)
            } else {
                None
            },
            max: if self.count > 0 && options.needs_max() {
                Some(self.max)
            } else {
                None
            },
            status: status.to_owned(),
            unit,
            histogram: options.histogram_edges.as_ref().map(|e| Histogram {
                edges: e.clone(),
                covered_cell_equivalents: self.hist.iter().map(|s| s.value()).collect(),
                underflow: self.under.value(),
                overflow: self.over.value(),
            }),
            weighted_sum: options.weight_band.map(|_| ws),
            weight_sum: options.weight_band.map(|_| ww),
            weighted_mean: if options.weight_band.is_some() && ww > 0. {
                Some(ws / ww)
            } else {
                None
            },
            weighted_valid_cell_equivalents: options
                .weight_band
                .map(|_| self.weighted_valid.value()),
            extensions: self.extensions.map_or_else(
                || Ok(crate::reducers::ReducerResult::default()),
                |r| r.finish(cancel),
            )?,
        })
    }
    fn full_range(
        &mut self,
        band: &Band,
        start: usize,
        end: usize,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut count = 0;
        for begin in (start..end).step_by(4096) {
            check_cancel(cancel)?;
            let stop = (begin + 4096).min(end);
            for (&value, &valid) in band.values[begin..stop]
                .iter()
                .zip(&band.valid[begin..stop])
            {
                if valid {
                    if self.reduce_sum {
                        self.sum.add(value);
                    }
                    count += 1;
                    if self.reduce_min {
                        self.min = self.min.min(value);
                    }
                    if self.reduce_max {
                        self.max = self.max.max(value);
                    }
                }
            }
        }
        // Full cells have integral support; count them exactly and add once.
        self.valid.add(count as f64);
        self.count += count;
        Ok(())
    }
}

pub fn measure(
    r: &Raster,
    p: &Plan,
    options: &Options,
    index: Option<&Prepared>,
    cancel: &AtomicBool,
) -> Result<Vec<BandResult>> {
    let bands = options.selected_bands(r.bands.len());
    measure_states(r, p, options, index, cancel)?
        .into_iter()
        .zip(bands)
        .map(|(acc, bi)| {
            acc.finish_cancellable(
                bi,
                p.selected,
                p.polygon_area,
                p.intersecting,
                r.bands[bi].unit.clone(),
                options,
                cancel,
            )
        })
        .collect()
}

pub(crate) fn measure_states(
    r: &Raster,
    p: &Plan,
    options: &Options,
    index: Option<&Prepared>,
    cancel: &AtomicBool,
) -> Result<Vec<Acc>> {
    check_cancel(cancel)?;
    options.validate(r.bands.len())?;
    ensure!(r.grid == p.grid, "coverage plan grid mismatch");
    if let Some(index) = index {
        ensure!(
            index.source_id == r.source_id && index.grid == r.grid,
            "stale index source/grid identity"
        );
    }
    let bands = if options.bands.is_empty() {
        (0..r.bands.len()).collect::<Vec<_>>()
    } else {
        options.bands.clone()
    };
    ensure!(
        bands.len() <= 20 && bands.iter().all(|&b| b < r.bands.len()),
        "band index out of range"
    );
    ensure!(
        bands
            .iter()
            .enumerate()
            .all(|(i, b)| !bands[..i].contains(b)),
        "duplicate bands not supported"
    );
    if let Some(w) = options.weight_band {
        ensure!(w < r.bands.len(), "weight band out of range");
    }
    if let Some(e) = &options.histogram_edges {
        ensure!(
            (2..=257).contains(&e.len())
                && e.iter().all(|v| v.is_finite())
                && e.windows(2).all(|w| w[0] < w[1]),
            "histogram edges must be finite strictly increasing, 2..257 entries"
        );
    }
    let mut results = Vec::new();
    let edges = options.histogram_edges.as_deref();
    let weights = options.weight_band.map(|b| &r.bands[b]);
    let can_index = options.summaries_eligible();
    let accumulator_bytes = Acc::retained_bytes(options)?
        .checked_mul(bands.len())
        .and_then(|n| n.checked_add(Acc::transient_bytes(options)))
        .ok_or_else(|| anyhow::anyhow!("accumulator budget overflow"))?;
    ensure!(
        r.bytes().saturating_add(accumulator_bytes) <= MAX_BYTES,
        "raster and reducer memory budget exceeded"
    );
    let per_row = r.grid.width.div_ceil(BLOCK);
    for &bi in &bands {
        let b = &r.bands[bi];
        let mut acc = Acc::new(edges.map_or(0, |e| e.len() - 1), options);
        for s in &p.spans {
            check_cancel(cancel)?;
            if can_index {
                let row_start = s.row * r.grid.width;
                if let Some(ix) = index {
                    let head = s.start.div_ceil(BLOCK).saturating_mul(BLOCK).min(s.end);
                    acc.full_range(b, row_start + s.start, row_start + head, cancel)?;
                    let mut col = head;
                    while col < s.end && (col + BLOCK).min(r.grid.width) <= s.end {
                        if col % 4096 == 0 {
                            check_cancel(cancel)?;
                        }
                        acc.block(&ix.blocks[bi][s.row * per_row + col / BLOCK]);
                        col = (col + BLOCK).min(r.grid.width);
                    }
                    acc.full_range(b, row_start + col, row_start + s.end, cancel)?;
                } else {
                    acc.full_range(b, row_start + s.start, row_start + s.end, cancel)?;
                }
                continue;
            }
            let mut col = s.start;
            while col < s.end {
                if col % 4096 == 0 {
                    check_cancel(cancel)?;
                }
                acc.cell(b, s.row * r.grid.width + col, 1., weights, edges);
                col += 1;
            }
        }
        for (n, c) in p.cells.iter().enumerate() {
            if n % 4096 == 0 {
                check_cancel(cancel)?;
            }
            acc.cell(b, c.row * r.grid.width + c.col, c.fraction, weights, edges);
        }
        results.push(acc);
    }
    Ok(results)
}

#[cfg(test)]
mod continuation_merge_tests {
    use super::*;
    use serde_json::json;

    fn opts(names: &[&str]) -> Options {
        Options {
            statistics: Some(names.iter().map(|s| s.to_string()).collect()),
            ..Options::default()
        }
    }
    fn result(acc: Acc, o: &Options) -> serde_json::Value {
        serde_json::to_value(acc.finish(0, 4., 4., 4, None, o).unwrap()).unwrap()
    }
    #[test]
    fn full_state_merges_preserve_compensation_moments_and_exact_ranks() {
        let o = opts(&["sum"]);
        let b = Band {
            values: vec![1e16, 1., -1e16],
            valid: vec![true; 3],
            unit: None,
        };
        let mut a = Acc::new(0, &o);
        let mut c = Acc::new(0, &o);
        a.cell(&b, 0, 1., None, None);
        a.cell(&b, 1, 1., None, None);
        c.cell(&b, 2, 1., None, None);
        a.merge_acc(c).unwrap();
        assert_eq!(result(a, &o)["fractional_sum"], 1.);

        let o = opts(&["variance", "stddev"]);
        let b = Band {
            values: vec![1e16, 1e16 + 2., 1e16 + 4., 1e16 + 6.],
            valid: vec![true; 4],
            unit: None,
        };
        let f = [1., 0.5, 0.25, 0.125];
        let mut whole = Acc::new(0, &o);
        let mut left = Acc::new(0, &o);
        let mut right = Acc::new(0, &o);
        for i in 0..4 {
            whole.cell(&b, i, f[i], None, None);
            if i < 2 {
                left.cell(&b, i, f[i], None, None)
            } else {
                right.cell(&b, i, f[i], None, None)
            }
        }
        left.merge_acc(right).unwrap();
        let w = result(whole, &o);
        let m = result(left, &o);
        assert!((w["variance"].as_f64().unwrap() - m["variance"].as_f64().unwrap()).abs() < 1e-12);

        let mut o = opts(&["majority", "variety", "median", "quantiles"]);
        o.category_values = Some(vec![10., 20.]);
        o.quantiles = Some(vec![0., 0.5, 1.]);
        let b = Band {
            values: vec![10., 20., 20.],
            valid: vec![true; 3],
            unit: None,
        };
        let mut a = Acc::new(0, &o);
        let mut c = Acc::new(0, &o);
        a.cell(&b, 0, 0.5, None, None);
        c.cell(&b, 1, 0.5, None, None);
        c.cell(&b, 2, 2f64.powi(-54), None, None);
        a.merge_acc(c).unwrap();
        let r = result(a, &o);
        assert_eq!(r["majority"], 20.);
        assert_eq!(r["median"], 20.);
        assert_eq!(r["quantiles"]["values"], json!([10., 20., 20.]));
    }
    #[test]
    fn full_state_merge_retains_rare_exact_weighted_central_state() {
        use num_rational::BigRational;
        use num_traits::ToPrimitive;
        let mut o = opts(&["weighted_variance"]);
        o.weight_band = Some(1);
        let band = Band {
            values: vec![0., 1e160, 1e160],
            valid: vec![true; 3],
            unit: None,
        };
        let weights = Band {
            values: vec![1., 1e-320, 1e-320],
            valid: vec![true; 3],
            unit: None,
        };
        let mut left = Acc::new(0, &o);
        let mut right = Acc::new(0, &o);
        left.cell(&band, 0, 1., Some(&weights), None);
        right.cell(&band, 1, 0.3, Some(&weights), None);
        right.cell(&band, 2, 0.7, Some(&weights), None);
        left.merge_acc(right).unwrap();
        let r = result(left, &o);
        let weight = BigRational::from_float(1e-320).unwrap()
            * (BigRational::from_float(0.3).unwrap() + BigRational::from_float(0.7).unwrap());
        let total = BigRational::from_integer(1.into()) + &weight;
        let value = BigRational::from_float(1e160).unwrap();
        let expected = (&value * &value * weight / (&total * &total))
            .to_f64()
            .unwrap();
        assert!((r["weighted_variance"].as_f64().unwrap() - expected).abs() <= expected * 1e-10);
        assert_eq!(r["moment_diagnostics"]["weighted_rational_fallback"], true);
    }
    #[test]
    fn merge_rejects_mismatched_state_and_global_quantile_limit() {
        let mut a = Acc::new(0, &opts(&["sum"]));
        let b = Acc::new(0, &opts(&["mean"]));
        assert!(a.merge_acc(b).is_err());
        let mut o = opts(&["median"]);
        o.quantile_max_samples = Some(1);
        let b = Band {
            values: vec![1.],
            valid: vec![true],
            unit: None,
        };
        let mut a = Acc::new(0, &o);
        let mut c = Acc::new(0, &o);
        a.cell(&b, 0, 1., None, None);
        c.cell(&b, 0, 1., None, None);
        assert!(
            a.merge_acc(c)
                .unwrap_err()
                .to_string()
                .contains("sample budget")
        );
        assert!(Acc::estimate_bytes(&o).unwrap() > std::mem::size_of::<Acc>());
    }
}
