//! A bounded, trusted tile expression language. No source-sized derived arrays.
use crate::model::{Band, Raster, check_cancel};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::atomic::AtomicBool};

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Band { band: usize },
    Constant { value: f64 },
    Add { left: Box<Expr>, right: Box<Expr> },
    Subtract { left: Box<Expr>, right: Box<Expr> },
    Multiply { left: Box<Expr>, right: Box<Expr> },
    Divide { left: Box<Expr>, right: Box<Expr> },
    NormalizedDifference { left: Box<Expr>, right: Box<Expr> },
    Greater { left: Box<Expr>, right: Box<Expr> },
    GreaterEqual { left: Box<Expr>, right: Box<Expr> },
    Less { left: Box<Expr>, right: Box<Expr> },
    And { left: Box<Expr>, right: Box<Expr> },
    Valid { input: Box<Expr> },
}
impl Expr {
    fn children(&self) -> Vec<&Expr> {
        match self {
            Self::Band { .. } | Self::Constant { .. } => vec![],
            Self::Valid { input } => vec![input],
            Self::Add { left, right }
            | Self::Subtract { left, right }
            | Self::Multiply { left, right }
            | Self::Divide { left, right }
            | Self::NormalizedDifference { left, right }
            | Self::Greater { left, right }
            | Self::GreaterEqual { left, right }
            | Self::Less { left, right }
            | Self::And { left, right } => vec![left, right],
        }
    }
    pub fn validate(&self, band_count: usize) -> Result<Vec<usize>> {
        let mut stack = vec![(self, 1)];
        let mut nodes = 0;
        let mut bands = BTreeSet::new();
        while let Some((node, depth)) = stack.pop() {
            nodes += 1;
            ensure!(
                nodes <= 64 && depth <= 12,
                "expression complexity budget exceeded"
            );
            match node {
                Self::Band { band } => {
                    ensure!(*band < band_count, "expression band out of range");
                    bands.insert(*band);
                }
                Self::Constant { value } => {
                    ensure!(value.is_finite(), "nonfinite expression constant")
                }
                _ => (),
            }
            stack.extend(node.children().into_iter().map(|c| (c, depth + 1)));
        }
        Ok(bands.into_iter().collect())
    }
    fn value(&self, raster: &Raster, mapping: &[usize], cell: usize) -> Option<f64> {
        let binary = |left: &Expr, right: &Expr| -> Option<(f64, f64)> {
            Some((
                left.value(raster, mapping, cell)?,
                right.value(raster, mapping, cell)?,
            ))
        };
        let result = match self {
            Self::Band { band } => {
                let b = &raster.bands[mapping.iter().position(|v| v == band)?];
                return b.valid[cell].then_some(b.values[cell]);
            }
            Self::Constant { value } => *value,
            Self::Valid { input } => {
                return Some(if input.value(raster, mapping, cell).is_some() {
                    1.
                } else {
                    0.
                });
            }
            Self::Add { left, right } => {
                let (a, b) = binary(left, right)?;
                a + b
            }
            Self::Subtract { left, right } => {
                let (a, b) = binary(left, right)?;
                a - b
            }
            Self::Multiply { left, right } => {
                let (a, b) = binary(left, right)?;
                a * b
            }
            Self::Divide { left, right } => {
                let (a, b) = binary(left, right)?;
                if b == 0. {
                    return None;
                }
                a / b
            }
            Self::NormalizedDifference { left, right } => {
                let (a, b) = binary(left, right)?;
                // Scaling prevents avoidable overflow in both sum and difference.
                let scale = a.abs().max(b.abs());
                if scale == 0. {
                    return None;
                }
                let (a, b) = (a / scale, b / scale);
                if a + b == 0. {
                    return None;
                }
                (a - b) / (a + b)
            }
            Self::Greater { left, right } => {
                let (a, b) = binary(left, right)?;
                if a > b { 1. } else { 0. }
            }
            Self::GreaterEqual { left, right } => {
                let (a, b) = binary(left, right)?;
                if a >= b { 1. } else { 0. }
            }
            Self::Less { left, right } => {
                let (a, b) = binary(left, right)?;
                if a < b { 1. } else { 0. }
            }
            Self::And { left, right } => {
                let (a, b) = binary(left, right)?;
                if a != 0. && b != 0. { 1. } else { 0. }
            }
        };
        result.is_finite().then_some(result)
    }
    /// A mask-only caller may reuse these admission bytes within one source
    /// window. The caller reserves the payload and Vec header before reading.
    pub(crate) fn evaluate_mask(
        &self,
        raster: &Raster,
        mapping: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        check_cancel(cancel)?;
        let count = raster.grid.cells()?;
        let mut mask = Vec::with_capacity(count);
        for i in 0..count {
            if i % 4096 == 0 {
                check_cancel(cancel)?;
            }
            mask.push(u8::from(
                self.value(raster, mapping, i)
                    .is_some_and(|value| value != 0.),
            ));
        }
        Ok(mask)
    }
    pub fn evaluate(
        &self,
        mask: Option<&Expr>,
        raster: &Raster,
        mapping: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Band> {
        let count = raster.grid.cells()?;
        let mut values = Vec::with_capacity(count);
        let mut valid = Vec::with_capacity(count);
        for i in 0..count {
            if i % 4096 == 0 {
                check_cancel(cancel)?;
            }
            let value = if mask.is_none_or(|m| m.value(raster, mapping, i).is_some_and(|v| v != 0.))
            {
                self.value(raster, mapping, i)
            } else {
                None
            };
            values.push(value.unwrap_or(0.));
            valid.push(value.is_some());
        }
        Ok(Band {
            values,
            valid,
            unit: None,
        })
    }
}
