use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;
pub const MAX_CELLS: usize = 100_000_000;
pub const MAX_VERTICES: usize = 10_000;
pub const MAX_ENTRIES: usize = 5_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub transform: [f64; 6],
    pub crs: String,
}
impl Grid {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.width > 0 && self.height > 0,
            "grid dimensions must be positive"
        );
        ensure!(
            self.width <= 10_000_000 && self.height <= 10_000_000,
            "grid dimension budget exceeded"
        );
        ensure!(
            self.transform.iter().all(|v| v.is_finite()),
            "nonfinite affine transform"
        );
        ensure!(
            self.transform[1] > 0.0
                && self.transform[5] < 0.0
                && self.transform[2] == 0.0
                && self.transform[4] == 0.0,
            "only north-up affine grids supported"
        );
        ensure!(!self.crs.trim().is_empty(), "explicit CRS required");
        ensure!(self.crs.len() <= 4096, "CRS metadata exceeds 4096 bytes");
        ensure!(
            self.transform[0] + self.transform[1] != self.transform[0]
                && self.transform[3] + self.transform[5] != self.transform[3],
            "grid precision cannot resolve a cell"
        );
        Ok(())
    }
    pub fn cells(&self) -> Result<usize> {
        self.width
            .checked_mul(self.height)
            .ok_or_else(|| anyhow::anyhow!("grid size overflow"))
    }
    pub fn identity(&self) -> String {
        blake3::hash(&serde_json::to_vec(self).expect("finite validated grid"))
            .to_hex()
            .to_string()
    }
}

#[derive(Debug)]
pub struct Band {
    pub values: Vec<f64>,
    pub valid: Vec<bool>,
    pub unit: Option<String>,
}
#[derive(Debug)]
pub struct Raster {
    pub grid: Grid,
    pub bands: Vec<Band>,
    pub source_id: String,
}
impl Raster {
    pub fn validate(&self) -> Result<()> {
        self.validate_band_limit(20)
    }
    /// Source-owned transient windows can be wider than the resident API when
    /// their reader explicitly admits them under its unchanged byte budget.
    pub(crate) fn validate_source_window(&self) -> Result<()> {
        self.validate_band_limit(64)
    }
    /// Structural postcondition for values constructed by the trusted SKV
    /// normalizer. Arbitrary reader results must use full validation instead.
    pub(crate) fn validate_source_window_structure(&self) -> Result<()> {
        self.validate_band_limit_with(64, |_| Ok(()))
    }
    fn validate_band_limit(&self, band_limit: usize) -> Result<()> {
        self.validate_band_limit_with(band_limit, |band| {
            ensure!(
                band.values
                    .iter()
                    .zip(&band.valid)
                    .all(|(v, m)| !m || v.is_finite()),
                "valid values must be finite"
            );
            Ok(())
        })
    }
    fn validate_band_limit_with(
        &self,
        band_limit: usize,
        validate_values: impl Fn(&Band) -> Result<()>,
    ) -> Result<()> {
        self.grid.validate()?;
        ensure!(
            self.source_id.len() <= 1024,
            "source identity exceeds 1024 bytes"
        );
        let n = self.grid.cells()?;
        ensure!(
            !self.bands.is_empty() && self.bands.len() <= band_limit,
            "1..{band_limit} bands required"
        );
        ensure!(
            n.checked_mul(self.bands.len())
                .and_then(|n| n.checked_mul(9))
                .is_some_and(|n| n <= MAX_BYTES),
            "raster memory budget exceeded"
        );
        for band in &self.bands {
            ensure!(
                band.unit.as_ref().is_none_or(|unit| unit.len() <= 1024),
                "band unit exceeds 1024 bytes"
            );
            ensure!(
                band.values.len() == n && band.valid.len() == n,
                "band dimensions do not match grid"
            );
            validate_values(band)?;
        }
        ensure!(self.bytes() <= MAX_BYTES, "raster memory budget exceeded");
        Ok(())
    }
    pub fn bytes(&self) -> usize {
        self.bands
            .iter()
            .map(|b| {
                b.values
                    .capacity()
                    .saturating_mul(8)
                    .saturating_add(b.valid.capacity())
                    .saturating_add(b.unit.as_ref().map_or(0, String::capacity))
            })
            .sum::<usize>()
            .saturating_add(self.bands.capacity() * std::mem::size_of::<Band>())
            .saturating_add(self.grid.crs.capacity())
            .saturating_add(self.source_id.capacity())
            .saturating_add(std::mem::size_of::<Raster>())
    }
    pub fn content_identity(&self) -> String {
        self.content_identity_cancellable(&AtomicBool::new(false))
            .expect("uncancelled identity")
    }
    pub fn content_identity_cancellable(&self, cancel: &AtomicBool) -> Result<String> {
        let mut hash = blake3::Hasher::new();
        hash.update(self.grid.identity().as_bytes());
        for b in &self.bands {
            hash.update(serde_json::to_string(&b.unit)?.as_bytes());
            for (i, (v, m)) in b.values.iter().zip(&b.valid).enumerate() {
                if i % 4096 == 0 {
                    check_cancel(cancel)?;
                }
                hash.update(&v.to_le_bytes());
                hash.update(&[*m as u8]);
            }
        }
        Ok(hash.finalize().to_hex().to_string())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RasterInput {
    pub grid: Grid,
    pub bands: Vec<BandInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BandInput {
    pub values: Vec<Option<f64>>,
    pub valid: Option<Vec<bool>>,
    pub nodata: Option<f64>,
    pub scale: Option<f64>,
    pub offset: Option<f64>,
    pub unit: Option<String>,
}
impl RasterInput {
    pub fn decode(self) -> Result<Raster> {
        self.decode_with_budget(MAX_BYTES, &AtomicBool::new(false))
    }
    pub fn decode_with_budget(self, max_bytes: usize, cancel: &AtomicBool) -> Result<Raster> {
        check_cancel(cancel)?;
        self.grid.validate()?;
        let n = self.grid.cells()?;
        ensure!(
            n.checked_mul(self.bands.len())
                .and_then(|n| n.checked_mul(9))
                .is_some_and(|n| n <= max_bytes.min(MAX_BYTES)),
            "raster memory budget exceeded"
        );
        let mut bands = Vec::new();
        for input in self.bands {
            ensure!(
                input.unit.as_ref().is_none_or(|unit| unit.len() <= 1024),
                "band unit exceeds 1024 bytes"
            );
            ensure!(input.values.len() == n, "band values length mismatch");
            ensure!(
                input.valid.as_ref().is_none_or(|m| m.len() == n),
                "mask length mismatch"
            );
            let (scale, offset) = (input.scale.unwrap_or(1.), input.offset.unwrap_or(0.));
            ensure!(
                scale.is_finite() && offset.is_finite(),
                "nonfinite scale/offset"
            );
            let mut values = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for (i, raw) in input.values.into_iter().enumerate() {
                if i % 4096 == 0 {
                    check_cancel(cancel)?;
                }
                let decoded = raw.map(|v| v * scale + offset);
                let good = raw.is_some_and(|v| v.is_finite() && Some(v) != input.nodata)
                    && input.valid.as_ref().is_none_or(|m| m[i])
                    && decoded.is_some_and(f64::is_finite);
                values.push(if good { decoded.unwrap() } else { 0. });
                valid.push(good);
            }
            bands.push(Band {
                values,
                valid,
                unit: input.unit,
            });
        }
        let mut raster = Raster {
            grid: self.grid,
            bands,
            source_id: String::new(),
        };
        raster.validate()?;
        raster.source_id = raster.content_identity_cancellable(cancel)?;
        ensure!(
            raster.bytes() <= max_bytes.min(MAX_BYTES),
            "raster memory budget exceeded including metadata"
        );
        Ok(raster)
    }
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    Ok(())
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Sum {
    total: f64,
    correction: f64,
}
impl Sum {
    pub fn parts(self) -> [f64; 2] {
        [self.total, self.correction]
    }
    pub fn from_parts(parts: [f64; 2]) -> Result<Self> {
        ensure!(
            parts.iter().all(|x| x.is_finite()) && (parts[0] + parts[1]).is_finite(),
            "nonfinite compensated state"
        );
        Ok(Self {
            total: parts[0],
            correction: parts[1],
        })
    }
    pub fn merge(&mut self, other: Self) {
        self.add(other.total);
        self.add(other.correction);
    }
    pub fn add(&mut self, x: f64) {
        let t = self.total + x;
        self.correction += if self.total.abs() >= x.abs() {
            (self.total - t) + x
        } else {
            (x - t) + self.total
        };
        self.total = t;
    }
    pub fn value(self) -> f64 {
        self.total + self.correction
    }
}

#[cfg(test)]
mod window_validation_tests {
    use super::*;

    fn raster(bands: usize) -> Raster {
        Raster {
            grid: Grid {
                width: 1,
                height: 1,
                transform: [0., 1., 0., 1., 0., -1.],
                crs: "LOCAL".into(),
            },
            bands: (0..bands)
                .map(|_| Band {
                    values: vec![1.],
                    valid: vec![true],
                    unit: None,
                })
                .collect(),
            source_id: "validation-fixture".into(),
        }
    }

    #[test]
    fn full_validators_keep_value_checks_and_distinct_band_limits() {
        let mut window = raster(21);
        assert!(window.validate().unwrap_err().to_string().contains("1..20"));
        window.validate_source_window().unwrap();
        window.validate_source_window_structure().unwrap();
        window.bands[20].values[0] = f64::INFINITY;
        assert!(
            window
                .validate_source_window()
                .unwrap_err()
                .to_string()
                .contains("finite")
        );
        window.validate_source_window_structure().unwrap();
        let mut resident = raster(1);
        resident.bands[0].values[0] = f64::NAN;
        assert!(
            resident
                .validate()
                .unwrap_err()
                .to_string()
                .contains("finite")
        );
        resident.bands[0].valid[0] = false;
        resident.validate().unwrap();
        resident.validate_source_window().unwrap();
        assert!(raster(65).validate_source_window_structure().is_err());
    }

    #[test]
    fn structural_validation_keeps_logical_budget_and_capacity_accounting() {
        let mut window = raster(64);
        // Reject the declared allocation before accessing the deliberately tiny
        // sample vectors; testing the fixed cap does not require a huge allocation.
        window.grid.width = 10_000_000;
        assert!(
            window
                .validate_source_window_structure()
                .unwrap_err()
                .to_string()
                .contains("raster memory budget exceeded")
        );
        let mut small = raster(1);
        small.bands[0].values.reserve_exact(1024);
        small.bands[0].valid.reserve_exact(2048);
        small.grid.crs.reserve_exact(512);
        small.source_id.reserve_exact(512);
        let payload_capacity =
            small.bands[0].values.capacity() * 8 + small.bands[0].valid.capacity();
        assert!(payload_capacity > 9);
        assert!(
            small.bytes()
                >= payload_capacity
                    + small.grid.crs.capacity()
                    + small.source_id.capacity()
                    + std::mem::size_of::<Raster>()
        );
        small.validate_source_window_structure().unwrap();
        small.validate_source_window().unwrap();
    }
}
