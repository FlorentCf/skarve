//! Optional upstream execution over borrowed Skarve source windows.
//! No source handle or caller buffer crosses the ABI without a synchronous lease.
use crate::{
    aggregate::Options,
    backend::{EeOptions, FIELDS},
    model::{Grid, check_cancel},
    source::WindowSource,
    tile_cache::TileCache,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

pub struct Input<'a> {
    pub source: &'a dyn WindowSource,
    pub bands: Vec<usize>,
}
pub struct Descriptor {
    pub source_id: String,
    pub grid: Grid,
    pub bands: Vec<usize>,
    pub units: Vec<Option<String>>,
    pub offset: usize,
}
pub struct Output {
    pub descriptors: Vec<Descriptor>,
    pub values: Vec<f64>,
    pub defined: Vec<u8>,
    pub band_count: usize,
    pub metrics: Value,
}
impl Output {
    pub fn bytes(&self) -> usize {
        self.values.capacity() * 8
            + self.defined.capacity()
            + self
                .descriptors
                .iter()
                .map(|d| d.source_id.len() + d.grid.crs.len() + d.bands.len() * 128 + 512)
                .sum::<usize>()
    }
    pub fn bands(&self, zone: usize, source: usize, statistics: &[String]) -> Vec<Value> {
        let d = &self.descriptors[source];
        d.bands.iter().enumerate().map(|(i,&band)| {
            let base=(zone*self.band_count+d.offset+i)*5;
            let mut row=json!({"band":band,"unit":d.units[i],"status":if self.values[base+1]>0. {"ok"} else {"no_valid_data"}});
            for (k,(stat,field)) in FIELDS.iter().zip(["fractional_sum","covered_cell_equivalents","coverage_weighted_mean","min","max"]).enumerate() {
                if statistics.iter().any(|s| s==stat) { row[field]=if self.defined[base+k]==0 {Value::Null} else {json!(self.values[base+k])}; }
            }
            row
        }).collect()
    }
}

pub fn statistics(options: &Options) -> Result<Vec<String>> {
    let selected = options
        .statistics
        .clone()
        .unwrap_or_else(|| FIELDS.iter().map(|s| s.to_string()).collect());
    ensure!(
        !selected.is_empty()
            && selected.len() <= 5
            && selected.iter().all(|s| FIELDS.contains(&s.as_str())),
        "exactextract supports sum/support/mean/min/max only; support is fractional, not integer count"
    );
    ensure!(
        options.histogram_edges.is_none()
            && options.weight_band.is_none()
            && options.category_values.is_none()
            && options.quantiles.is_none()
            && options.quantile_max_samples.is_none(),
        "extended or weighted reducers are unavailable in the exactextract backend"
    );
    ensure!(
        (0..selected.len()).all(|i| !selected[..i].contains(&selected[i])),
        "duplicate exactextract statistic"
    );
    Ok(selected)
}

/// Validate using the ordinary geometry contract, then encode the ORIGINAL world
/// coordinates once. Round-tripping through pixel coordinates would change them.
fn geometry_wkb(v: &Value, grid: &Grid, cancel: &AtomicBool) -> Result<Vec<u8>> {
    crate::coverage::geometry(v, grid, cancel)?;
    fn polygon(v: &Value, out: &mut Vec<u8>) {
        out.push(1);
        out.extend(3_u32.to_le_bytes());
        let rings = v.as_array().expect("validated rings");
        out.extend((rings.len() as u32).to_le_bytes());
        for ring in rings {
            let points = ring.as_array().expect("validated points");
            out.extend((points.len() as u32).to_le_bytes());
            for p in points {
                out.extend(p[0].as_f64().expect("validated x").to_le_bytes());
                out.extend(p[1].as_f64().expect("validated y").to_le_bytes());
            }
        }
    }
    let mut bytes = Vec::new();
    if v["type"] == "Polygon" {
        polygon(&v["coordinates"], &mut bytes)
    } else {
        bytes.push(1);
        bytes.extend(6_u32.to_le_bytes());
        let parts = v["coordinates"].as_array().expect("validated parts");
        bytes.extend((parts.len() as u32).to_le_bytes());
        for part in parts {
            polygon(part, &mut bytes);
        }
    }
    Ok(bytes)
}

/// This deliberately bounded contract matches unscaled Rasterio arrays. It does
/// not reinterpret legacy normalized sources or reconstruct raw bits from them.
fn rasterio_metadata(source: &dyn WindowSource) -> Result<&crate::source::RawRasterMetadata> {
    use crate::source::RawScalarType;
    let raw = source.raw_metadata().ok_or_else(|| {
        anyhow::anyhow!("exactextract_rasterio_v030 requires original typed source access")
    })?;
    let metadata = source.metadata();
    ensure!(
        !raw.bands.is_empty() && raw.bands.len() == metadata.bands.len(),
        "Rasterio compatibility requires matching exposed typed band metadata"
    );
    let nodata = raw.bands[0].nodata_f64_bits;
    for (band, interpreted) in raw.bands.iter().zip(&metadata.bands) {
        // exactextract0.3.0 RasterView copies NoData but not an independent
        // mask, so matching that adapter is only safe for these mask contracts.
        ensure!(
            matches!(band.mask_flags, 1 | 8),
            "exactextract_rasterio_v030 requires all-valid or NoData-derived masks; explicit/alpha masks cannot guarantee upstream equivalence"
        );
        ensure!(
            band.mask_flags != 8 || band.nodata_f64_bits.is_some(),
            "Rasterio compatibility NoData-derived mask lacks NoData metadata"
        );
        ensure!(
            matches!(
                band.scalar_type,
                RawScalarType::Float32 | RawScalarType::Float64
            ),
            "exactextract_rasterio_v030 currently requires Float32 or Float64 bands"
        );
        ensure!(
            f64::from_bits(band.scale_f64_bits) == 1.0
                && f64::from_bits(band.offset_f64_bits) == 0.0
                && interpreted.scale == 1.0
                && interpreted.offset == 0.0,
            "exactextract_rasterio_v030 requires identity scale/offset; scaled arrays have a different dtype rounding contract"
        );
        ensure!(
            band.nodata_f64_bits == nodata
                && interpreted.nodata.map(f64::to_bits) == band.nodata_f64_bits,
            "exactextract_rasterio_v030 requires uniform exposed-band NoData metadata"
        );
    }
    Ok(raw)
}

/// Compute exactly the north-up Rasterio bounds arithmetic, then res(). Keeping
/// this separate from Grid preserves the original source grid and its identity.
fn coverage_grid(grid: &Grid, rasterio: bool) -> Result<[f64; 6]> {
    let [xmin, _, _, ymax, _, _] = grid.transform;
    let ymin = ymax + grid.height as f64 * grid.transform[5];
    let xmax = xmin + grid.width as f64 * grid.transform[1];
    let dx = if rasterio {
        (xmax - xmin) / grid.width as f64
    } else {
        grid.transform[1]
    };
    let dy = if rasterio {
        (ymax - ymin) / grid.height as f64
    } else {
        -grid.transform[5]
    };
    ensure!(
        [xmin, ymin, xmax, ymax, dx, dy]
            .iter()
            .all(|v| v.is_finite())
            && dx > 0.0
            && dy > 0.0,
        "unrepresentable exactextract coverage grid"
    );
    Ok([xmin, ymin, xmax, ymax, dx, dy])
}

#[cfg(feature = "exactextract")]
fn rasterio_output_bound(w: usize, h: usize) -> Result<usize> {
    w.checked_mul(h)
        .and_then(|n| n.checked_mul(9))
        .and_then(|n| n.checked_add(crate::source::source_window_overhead(1)))
        .ok_or_else(|| anyhow::anyhow!("Rasterio callback output bound overflow"))
}

#[cfg(feature = "exactextract")]
#[allow(clippy::too_many_arguments)]
fn rasterio_window(
    source: &dyn WindowSource,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    band: usize,
    available: usize,
    cancel: &AtomicBool,
) -> Result<(crate::model::Raster, crate::source::ReadMetrics)> {
    use crate::{
        model::{Band, Raster},
        source::RawScalarType,
    };
    let metadata = source.metadata();
    // execute() already admitted all immutable source metadata before any read.
    let raw_metadata = source
        .raw_metadata()
        .ok_or_else(|| anyhow::anyhow!("typed metadata disappeared"))?;
    let info = &raw_metadata.bands[band];
    let output_bound = rasterio_output_bound(w, h)?;
    let raw_bound = source.raw_read_buffer_bound(w, h, &[band])?;
    ensure!(
        raw_bound
            .checked_add(output_bound)
            .is_some_and(|n| n <= available),
        "Rasterio callback raw plus interpreted buffers exceed window_bytes"
    );
    let (raw, mut metrics) = source.read_raw_selected_window_cancellable(
        x,
        y,
        w,
        h,
        &[band],
        available - output_bound,
        cancel,
    )?;
    let started = std::time::Instant::now();
    check_cancel(cancel)?;
    ensure!(
        raw.width == w && raw.height == h && raw.bands.len() == 1,
        "Rasterio callback returned unexpected typed dimensions"
    );
    let cells = w
        .checked_mul(h)
        .ok_or_else(|| anyhow::anyhow!("typed window overflow"))?;
    let input = &raw.bands[0];
    let scalar_width = info.scalar_type.byte_width();
    ensure!(
        input.mask.len() == cells
            && cells.checked_mul(scalar_width) == Some(input.samples_le.len()),
        "Rasterio callback typed sample or mask length mismatch"
    );
    // The upstream Python binding casts NoData to the array scalar type before
    // AbstractRaster<T>::get compares it. Float32 casting must happen first.
    let nodata = info.nodata_f64_bits.map(|bits| {
        let v = f64::from_bits(bits);
        if info.scalar_type == RawScalarType::Float32 {
            (v as f32) as f64
        } else {
            v
        }
    });
    let mut output = Band {
        values: vec![0.0; cells],
        valid: vec![false; cells],
        unit: info.unit.clone(),
    };
    for (cell, (bytes, &mask)) in input
        .samples_le
        .chunks_exact(scalar_width)
        .zip(&input.mask)
        .enumerate()
    {
        if cell % 4096 == 0 {
            check_cancel(cancel)?;
        }
        let value = match info.scalar_type {
            RawScalarType::Float32 => {
                f32::from_le_bytes(bytes.try_into().expect("checked scalar width")) as f64
            }
            RawScalarType::Float64 => {
                f64::from_le_bytes(bytes.try_into().expect("checked scalar width"))
            }
            _ => unreachable!("metadata eligibility checked"),
        };
        let invalid_value = value.is_nan() || nodata.is_some_and(|n| value == n);
        ensure!(
            mask != 0 || (info.mask_flags == 8 && invalid_value),
            "exactextract_rasterio_v030 raw mask contradicts its all-valid/NoData-derived metadata"
        );
        let valid = mask != 0 && !invalid_value;
        ensure!(
            !valid || value.is_finite(),
            "exactextract_rasterio_v030 cannot return unmasked infinite samples; complete result rejected"
        );
        if valid {
            // No multiply/add for identity scaling: preserve signed zero.
            output.values[cell] = value;
            output.valid[cell] = true;
        }
    }
    let mut grid = metadata.grid.clone();
    grid.width = w;
    grid.height = h;
    grid.transform[0] += x as f64 * grid.transform[1];
    grid.transform[3] += y as f64 * grid.transform[5];
    let raster = Raster {
        grid,
        bands: vec![output],
        source_id: metadata.source_id.clone(),
    };
    ensure!(
        raster.bytes() <= output_bound,
        "Rasterio callback output exceeds reserved metadata bound"
    );
    check_cancel(cancel)?;
    metrics.normalization_ms += started.elapsed().as_secs_f64() * 1000.0;
    Ok((raster, metrics))
}

#[allow(clippy::too_many_arguments)]
pub fn execute(
    inputs: &[Input<'_>],
    zones: &[Value],
    crs: &str,
    options: &Options,
    limits: &EeOptions,
    cancel: &AtomicBool,
    cache: &mut TileCache,
    available: usize,
) -> Result<Output> {
    check_cancel(cancel)?;
    limits.validate()?;
    let selected = statistics(options)?;
    let statistics_mask = FIELDS.iter().enumerate().fold(0_u32, |mask, (i, field)| {
        mask | if selected.iter().any(|s| s == field) {
            1 << i
        } else {
            0
        }
    });
    ensure!(
        cfg!(feature = "exactextract"),
        "exactextract backend is not installed"
    );
    ensure!(
        (1..=32).contains(&inputs.len()) && (1..=512).contains(&zones.len()),
        "exactextract requires 1..32 sources and 1..512 zones per bounded job"
    );
    let mut descriptors = Vec::new();
    let mut count = 0;
    for input in inputs {
        let meta = input.source.metadata();
        meta.grid.validate()?;
        ensure!(
            meta.grid.crs == crs,
            "polygon/source CRS mismatch; no implicit reprojection"
        );
        ensure!(
            meta.grid == inputs[0].source.metadata().grid,
            "exactextract all-source execution requires identical native grids; no implicit resampling or union-grid expansion"
        );
        let mut o = options.clone();
        o.bands = input.bands.clone();
        // Logical inputs may expose up to64 bands; validate each bounded20-band
        // group without widening resident Raster or reader allocations. The
        // native bridge already requests one logical band per window lease.
        crate::stored_summary::selected_bands(&o, meta.bands.len())?;
        ensure!(
            !input.bands.is_empty(),
            "exactextract requires selected bands"
        );
        if limits.rasterio_compatible {
            rasterio_metadata(input.source)?;
            coverage_grid(&meta.grid, true)?;
        }
        input.source.verify_immutable()?;
        descriptors.push(Descriptor {
            source_id: meta.source_id.clone(),
            grid: meta.grid.clone(),
            bands: input.bands.clone(),
            units: input
                .bands
                .iter()
                .map(|&b| meta.bands[b].unit.clone())
                .collect(),
            offset: count,
        });
        count += input.bands.len();
    }
    ensure!(count <= 64, "exactextract logical band budget is 64");
    let length = zones
        .len()
        .checked_mul(count)
        .and_then(|n| n.checked_mul(5))
        .ok_or_else(|| anyhow::anyhow!("exactextract result size overflow"))?;
    let payload = length
        .checked_mul(9)
        .ok_or_else(|| anyhow::anyhow!("exactextract result byte overflow"))?;
    // C++ allocations are not a hard RSS cap. Reserve explicitly admitted source,
    // geometry and output work in addition to the upstream chunk working allowance.
    ensure!(
        payload.saturating_add(count * 4096) <= limits.output_bytes,
        "exactextract complete typed output exceeds output_bytes"
    );
    ensure!(
        limits
            .window_bytes
            .saturating_add(limits.output_bytes)
            .saturating_add(128 << 20)
            <= available,
        "insufficient session memory for embedded exactextract admission"
    );
    let mut wkbs = Vec::new();
    let mut geometry_bytes = 0_usize;
    for zone in zones {
        check_cancel(cancel)?;
        let b = geometry_wkb(zone, &descriptors[0].grid, cancel)?;
        geometry_bytes = geometry_bytes.saturating_add(b.len() * 4);
        ensure!(
            geometry_bytes <= 64 << 20,
            "exactextract geometry staging exceeds 64 MiB"
        );
        wkbs.push(b);
    }
    #[cfg(feature = "exactextract")]
    {
        let mut output = ffi::run(
            inputs,
            descriptors,
            wkbs,
            count,
            length,
            statistics_mask,
            limits,
            cancel,
            cache,
        )?;
        for input in inputs {
            check_cancel(cancel)?;
            input.source.verify_immutable()?;
        }
        output.metrics["geometry_staging_bound_bytes"] = json!(geometry_bytes);
        output.metrics["typed_output_bytes"] = json!(payload);
        Ok(output)
    }
    #[cfg(not(feature = "exactextract"))]
    {
        let _ = (cache, wkbs, descriptors, statistics_mask);
        anyhow::bail!("exactextract backend is not installed")
    }
}

#[cfg(feature = "exactextract")]
mod ffi {
    use super::*;
    use crate::{
        model::Raster,
        tile_cache::{source_key, window_key},
    };
    use serde::Serialize;
    use std::{
        ffi::{CStr, c_char, c_void},
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{Arc, atomic::Ordering},
        time::Instant,
    };
    #[repr(C)]
    struct CGrid {
        xmin: f64,
        ymin: f64,
        xmax: f64,
        ymax: f64,
        dx: f64,
        dy: f64,
        width: u64,
        height: u64,
    }
    #[repr(C)]
    struct Wkb {
        data: *const u8,
        length: usize,
    }
    #[repr(C)]
    struct Window {
        values: *const f64,
        valid: *const u8,
        length: usize,
        lease: *mut c_void,
    }
    #[repr(C)]
    struct Request {
        abi_version: u32,
        strategy: u32,
        sources: *const CGrid,
        source_count: usize,
        features: *const Wkb,
        feature_count: usize,
        max_cells: u64,
        max_live_window_bytes: u64,
        context: *mut c_void,
        read: unsafe extern "C" fn(
            *mut c_void,
            usize,
            u64,
            u64,
            u64,
            u64,
            *mut Window,
            *mut c_char,
            usize,
        ) -> i32,
        release: unsafe extern "C" fn(*mut c_void, *mut c_void),
        cancelled: unsafe extern "C" fn(*mut c_void) -> i32,
        statistics_mask: u32,
        reserved: u32,
    }
    #[repr(C)]
    #[derive(Default, Serialize)]
    struct Metrics {
        read_calls: u64,
        read_cells: u64,
        read_bytes: u64,
        peak_live_window_bytes: u64,
        callback_nanoseconds: u64,
        upstream_nanoseconds: u64,
        total_nanoseconds: u64,
    }
    unsafe extern "C" {
        fn skarve_ee_execute_v2(
            request: *const Request,
            values: *mut f64,
            defined: *mut u8,
            length: usize,
            metrics: *mut Metrics,
            error: *mut c_char,
            capacity: usize,
        ) -> i32;
        fn skarve_ee_geos_version() -> *const c_char;
        fn skarve_ee_upstream_commit() -> *const c_char;
    }
    struct Context<'a> {
        inputs: &'a [Input<'a>],
        logical: Vec<(usize, usize)>,
        keys: Vec<String>,
        cache: &'a mut TileCache,
        limits: &'a EeOptions,
        cancel: &'a AtomicBool,
        windows: usize,
        decoded_bytes: u64,
        live: usize,
        peak: usize,
        peak_read_buffer_bound: usize,
        decode_ms: f64,
        normalization_ms: f64,
        raster_io: usize,
        callback_error: Option<String>,
    }
    struct Lease {
        raster: Arc<Raster>,
        charge: usize,
    }
    fn error_buffer(message: &str, p: *mut c_char, n: usize) {
        if n == 0 || p.is_null() {
            return;
        }
        let len = message.len().min(n - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(message.as_ptr(), p.cast(), len);
            *p.add(len) = 0;
        }
    }
    impl Context<'_> {
        fn read(&mut self, logical: usize, x: u64, y: u64, w: u64, h: u64) -> Result<Window> {
            check_cancel(self.cancel)?;
            ensure!(
                logical < self.logical.len(),
                "invalid upstream logical band"
            );
            let (group, band) = self.logical[logical];
            let source = self.inputs[group].source;
            let grid = &source.metadata().grid;
            let [x, y, w, h] = [x, y, w, h]
                .map(usize::try_from)
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()?
                .try_into()
                .expect("four dimensions");
            ensure!(
                w > 0
                    && h > 0
                    && x.checked_add(w).is_some_and(|v| v <= grid.width)
                    && y.checked_add(h).is_some_and(|v| v <= grid.height),
                "upstream window exceeds registered source"
            );
            self.windows = self
                .windows
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("window count overflow"))?;
            ensure!(
                self.windows <= self.limits.max_windows,
                "exactextract cumulative window budget exceeded"
            );
            let key = window_key(&self.keys[group], [x, y, w, h], &[band]);
            let raster = if let Some(cached) = self.cache.get(&key) {
                cached
            } else {
                let required = if self.limits.rasterio_compatible {
                    source
                        .raw_read_buffer_bound(w, h, &[band])?
                        .checked_add(rasterio_output_bound(w, h)?)
                        .ok_or_else(|| anyhow::anyhow!("Rasterio callback read bound overflow"))?
                } else {
                    source.read_buffer_bound(w, h, &[band])?
                };
                ensure!(
                    required <= self.limits.window_bytes.saturating_sub(self.live),
                    "exactextract callback read buffer exceeds window_bytes"
                );
                self.peak_read_buffer_bound = self.peak_read_buffer_bound.max(self.live + required);
                let bytes = w
                    .checked_mul(h)
                    .and_then(|n| n.checked_mul(9))
                    .ok_or_else(|| anyhow::anyhow!("window byte overflow"))?
                    as u64;
                self.decoded_bytes = self
                    .decoded_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow::anyhow!("decoded byte overflow"))?;
                ensure!(
                    self.decoded_bytes <= self.limits.decoded_bytes,
                    "exactextract cumulative decoded byte budget exceeded"
                );
                let available = self.limits.window_bytes.saturating_sub(self.live);
                let (r, m) = if self.limits.rasterio_compatible {
                    rasterio_window(source, x, y, w, h, band, available, self.cancel)?
                } else {
                    source.read_selected_window_cancellable(
                        x,
                        y,
                        w,
                        h,
                        &[band],
                        available,
                        self.cancel,
                    )?
                };
                self.decode_ms += m.read_decode_ms;
                self.normalization_ms += m.normalization_ms;
                self.raster_io += m.raster_io_calls;
                let r = Arc::new(r);
                self.cache.insert(key, Arc::clone(&r));
                r
            };
            check_cancel(self.cancel)?;
            ensure!(
                raster.bands.len() == 1 && raster.grid.width == w && raster.grid.height == h,
                "source callback returned unexpected dimensions"
            );
            let charge = raster.bytes();
            ensure!(
                charge <= self.limits.window_bytes.saturating_sub(self.live),
                "exactextract active window leases exceed window_bytes"
            );
            self.live += charge;
            self.peak = self.peak.max(self.live);
            let b = &raster.bands[0];
            // Rust bool occupies one byte with valid representations 0 and 1.
            // The immutable raster lease keeps both arrays live until C++ release.
            let values = b.values.as_ptr();
            let valid = b.valid.as_ptr().cast::<u8>();
            let length = b.values.len();
            let lease = Box::into_raw(Box::new(Lease { raster, charge })).cast();
            Ok(Window {
                values,
                valid,
                length,
                lease,
            })
        }
    }
    unsafe extern "C" fn read(
        ctx: *mut c_void,
        band: usize,
        x: u64,
        y: u64,
        w: u64,
        h: u64,
        out: *mut Window,
        error: *mut c_char,
        capacity: usize,
    ) -> i32 {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = unsafe { &mut *ctx.cast::<Context<'_>>() };
            ctx.read(band, x, y, w, h)
        }));
        match result {
            Ok(Ok(window)) => {
                unsafe { out.write(window) };
                0
            }
            other => {
                let message = match other {
                    Ok(Err(e)) => e.to_string(),
                    _ => "Rust source callback panic contained".into(),
                };
                error_buffer(&message, error, capacity);
                unsafe { &mut *ctx.cast::<Context<'_>>() }.callback_error = Some(message);
                1
            }
        }
    }
    unsafe extern "C" fn release(ctx: *mut c_void, lease: *mut c_void) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let lease = unsafe { Box::from_raw(lease.cast::<Lease>()) };
            let ctx = unsafe { &mut *ctx.cast::<Context<'_>>() };
            ctx.live = ctx.live.saturating_sub(lease.charge);
            drop(lease.raster);
        }));
    }
    unsafe extern "C" fn cancelled(ctx: *mut c_void) -> i32 {
        i32::from(
            unsafe { &*ctx.cast::<Context<'_>>() }
                .cancel
                .load(Ordering::Relaxed),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run(
        inputs: &[Input<'_>],
        descriptors: Vec<Descriptor>,
        wkbs: Vec<Vec<u8>>,
        band_count: usize,
        length: usize,
        statistics_mask: u32,
        limits: &EeOptions,
        cancel: &AtomicBool,
        cache: &mut TileCache,
    ) -> Result<Output> {
        let start = Instant::now();
        let before = cache.stats();
        let mut grids = Vec::new();
        let mut logical = Vec::new();
        let mut keys = Vec::new();
        for (si, input) in inputs.iter().enumerate() {
            let g = &input.source.metadata().grid;
            let key = source_key(input.source)?;
            keys.push(if limits.rasterio_compatible {
                format!("rasterio_unscaled_raw_v030:{key}")
            } else {
                key
            });
            let [xmin, ymin, xmax, ymax, dx, dy] = coverage_grid(g, limits.rasterio_compatible)?;
            for &band in &input.bands {
                logical.push((si, band));
                grids.push(CGrid {
                    xmin,
                    ymin,
                    xmax,
                    ymax,
                    dx,
                    dy,
                    width: g.width as u64,
                    height: g.height as u64,
                });
            }
        }
        let features: Vec<_> = wkbs
            .iter()
            .map(|b| Wkb {
                data: b.as_ptr(),
                length: b.len(),
            })
            .collect();
        let mut context = Context {
            inputs,
            logical,
            keys,
            cache,
            limits,
            cancel,
            windows: 0,
            decoded_bytes: 0,
            live: 0,
            peak: 0,
            peak_read_buffer_bound: 0,
            decode_ms: 0.,
            normalization_ms: 0.,
            raster_io: 0,
            callback_error: None,
        };
        let request = Request {
            abi_version: 2,
            strategy: u32::from(limits.strategy == "raster-sequential"),
            sources: grids.as_ptr(),
            source_count: grids.len(),
            features: features.as_ptr(),
            feature_count: features.len(),
            max_cells: limits.max_cells_in_memory as u64,
            max_live_window_bytes: limits.window_bytes as u64,
            context: (&mut context as *mut Context<'_>).cast(),
            read,
            release,
            cancelled,
            statistics_mask,
            reserved: 0,
        };
        let mut values = vec![0.; length];
        let mut defined = vec![0; length];
        let mut metrics = Metrics::default();
        let mut error = [0_i8; 2048];
        let status = unsafe {
            skarve_ee_execute_v2(
                &request,
                values.as_mut_ptr(),
                defined.as_mut_ptr(),
                length,
                &mut metrics,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(
            context.live == 0,
            "exactextract bridge did not release all source leases"
        );
        if status != 0 {
            let reason = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
            anyhow::bail!("exactextract operation failed (status {status}): {reason}");
        }
        check_cancel(cancel)?;
        ensure!(
            context.callback_error.is_none(),
            "source callback failure invalidates complete exactextract output"
        );
        ensure!(
            values
                .iter()
                .zip(&defined)
                .all(|(v, &d)| d <= 1 && (d == 0 || v.is_finite())),
            "nonfinite or invalid exactextract output; complete result discarded"
        );
        let after = context.cache.stats();
        let work = json!({"bridge_abi_version":2,"source_interpretation":if limits.rasterio_compatible {"rasterio_unscaled_raw_v030"} else {"skarve_normalized_f64_v1"},"coverage_grid_interpretation":if limits.rasterio_compatible {"rasterio_bounds_resolution_v030"} else {"native_affine_resolution"},"statistics_mask":statistics_mask,"bridge":metrics,"strategy":limits.strategy,"source_callback_windows":context.windows,"decoded_value_bytes":context.decoded_bytes,"callback_raster_io_calls":context.raster_io,"read_decode_ms":context.decode_ms,"normalization_ms":context.normalization_ms,"peak_callback_accounted_bytes":context.peak,"peak_callback_read_buffer_bound_bytes":context.peak_read_buffer_bound,"decoded_cache_hits":after.hits-before.hits,"decoded_cache_misses":after.misses-before.misses,"decoded_cache_evictions":after.evictions-before.evictions,"decoded_cache_resident_bytes":context.cache.bytes(),"pixel_copy_bytes_in_cpp":0,"rust_dispatch_ms":start.elapsed().as_secs_f64()*1000.,"upstream_commit":unsafe{CStr::from_ptr(skarve_ee_upstream_commit())}.to_string_lossy(),"geos_version":unsafe{CStr::from_ptr(skarve_ee_geos_version())}.to_string_lossy(),"concurrency":"serialized optional calls; cooperative lock wait","timing_relationship":"upstream includes callbacks/sink; bridge total includes upstream; do not add overlapping spans"});
        Ok(Output {
            descriptors,
            values,
            defined,
            band_count,
            metrics: work,
        })
    }
}

#[cfg(all(test, feature = "exactextract"))]
mod rasterio_tests {
    use super::*;
    use crate::{
        backend::{self, EXACTEXTRACT_POLICY, EXACTEXTRACT_RASTERIO_POLICY},
        model::{Raster, check_cancel},
        source::{
            BandMetadata, RasterMetadata, RawBandMetadata, RawBandWindow, RawRasterMetadata,
            RawScalarType, RawWindow, ReadMetrics,
        },
    };
    use std::cell::Cell;
    use std::sync::atomic::Ordering;

    struct Source {
        metadata: RasterMetadata,
        raw: RawRasterMetadata,
        values: Vec<f64>,
        mask: Vec<u8>,
        reads: Cell<usize>,
        normalized_reads: Cell<usize>,
        cancel_after_read: bool,
        malformed: bool,
    }
    impl Source {
        fn new(kind: RawScalarType, values: Vec<f64>, mask: Vec<u8>) -> Self {
            let width = values.len();
            Self {
                metadata: RasterMetadata {
                    grid: Grid {
                        width,
                        height: 1,
                        transform: [0., 1., 0., 1., 0., -1.],
                        crs: "LOCAL".into(),
                    },
                    bands: vec![BandMetadata {
                        data_type: format!("{kind:?}"),
                        nodata: Some(-9999.),
                        scale: 1.,
                        offset: 0.,
                        unit: Some("people".into()),
                        block_size: (width, 1),
                    }],
                    source_id: "rasterio-typed-fixture".into(),
                },
                raw: RawRasterMetadata {
                    bands: vec![RawBandMetadata {
                        scalar_type: kind,
                        nodata_f64_bits: Some((-9999_f64).to_bits()),
                        scale_f64_bits: 1_f64.to_bits(),
                        offset_f64_bits: 0,
                        unit: Some("people".into()),
                        mask_flags: 8,
                        original_band_index: 0,
                        description: String::new(),
                    }],
                    pixel_convention: "Area".into(),
                    source_band_count: 1,
                    source_overview: None,
                },
                values,
                mask,
                reads: Cell::new(0),
                normalized_reads: Cell::new(0),
                cancel_after_read: false,
                malformed: false,
            }
        }
        fn window(&self, x: usize, y: usize, w: usize, h: usize) -> RawWindow {
            assert_eq!((y, h), (0, 1));
            let mut samples_le = Vec::new();
            for &v in &self.values[x..x + w] {
                if self.raw.bands[0].scalar_type == RawScalarType::Float32 {
                    samples_le.extend_from_slice(&(v as f32).to_le_bytes());
                } else {
                    samples_le.extend_from_slice(&v.to_le_bytes());
                }
            }
            let mut mask = self.mask[x..x + w].to_vec();
            if self.malformed {
                mask.pop();
            }
            RawWindow {
                width: w,
                height: h,
                bands: vec![RawBandWindow { samples_le, mask }],
            }
        }
    }
    impl WindowSource for Source {
        fn metadata(&self) -> &RasterMetadata {
            &self.metadata
        }
        fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
            Some(&self.raw)
        }
        fn verify_immutable(&self) -> Result<()> {
            Ok(())
        }
        fn raw_read_buffer_bound(&self, w: usize, h: usize, b: &[usize]) -> Result<usize> {
            ensure!(b == [0], "test band");
            Ok(w * h * 9 + 1024)
        }
        fn read_raw_selected_window_cancellable(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            b: &[usize],
            max: usize,
            c: &AtomicBool,
        ) -> Result<(RawWindow, ReadMetrics)> {
            check_cancel(c)?;
            ensure!(
                self.raw_read_buffer_bound(w, h, b)? <= max,
                "test raw bound"
            );
            self.reads.set(self.reads.get() + 1);
            let result = self.window(x, y, w, h);
            if self.cancel_after_read {
                c.store(true, Ordering::Relaxed);
            }
            Ok((result, ReadMetrics::default()))
        }
        fn read_selected_window_cancellable(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            b: &[usize],
            max: usize,
            c: &AtomicBool,
        ) -> Result<(Raster, ReadMetrics)> {
            self.normalized_reads.set(self.normalized_reads.get() + 1);
            let raster =
                self.window(x, y, w, h)
                    .normalize(&self.raw, &self.metadata, x, y, b, max, c)?;
            Ok((raster, ReadMetrics::default()))
        }
    }
    fn run(
        source: &Source,
        policy: &str,
        strategy: &str,
        cache: &mut TileCache,
        cancel: &AtomicBool,
        window_bytes: usize,
    ) -> Result<Output> {
        let options=backend::resolve(&json!({"backend":"exactextract","numerical_policy":policy,"backend_options":{"strategy":strategy,"window_bytes":window_bytes}}),false)?.options;
        let width = source.metadata.grid.width;
        let geometry =
            json!({"type":"Polygon","coordinates":[[[0,0],[width,0],[width,1],[0,1],[0,0]]]});
        execute(
            &[Input {
                source,
                bands: vec![0],
            }],
            &[geometry],
            "LOCAL",
            &Options::default(),
            &options,
            cancel,
            cache,
            512 << 20,
        )
    }
    #[test]
    fn bounds_resolution_preserves_the_source_grid_and_real36_rounding() {
        let step = f64::from_bits(0x3f4b4e81b312fb51);
        let grid = Grid {
            width: 512,
            height: 512,
            transform: [4.252499262989997, step, 0., 51.077500131689995, 0., -step],
            crs: "EPSG:4326".into(),
        };
        let original = grid.clone();
        let legacy = coverage_grid(&grid, false).unwrap();
        let rasterio = coverage_grid(&grid, true).unwrap();
        assert_eq!(legacy[..4], rasterio[..4]);
        assert_eq!(legacy[4].to_bits(), step.to_bits());
        assert_eq!(rasterio[4].to_bits(), step.to_bits() - 1);
        assert_eq!(rasterio[5].to_bits(), step.to_bits() + 47);
        assert_eq!(grid, original);
    }
    #[test]
    fn legacy_new_legacy_cache_isolation_preserves_signed_zero() {
        for kind in [RawScalarType::Float32, RawScalarType::Float64] {
            for strategy in ["feature-sequential", "raster-sequential"] {
                let source = Source::new(kind, vec![-0.0], vec![255]);
                let cancel = AtomicBool::new(false);
                let mut cache = TileCache::default();
                cache.set_limit(1 << 20).unwrap();
                let before = run(
                    &source,
                    EXACTEXTRACT_POLICY,
                    strategy,
                    &mut cache,
                    &cancel,
                    64 << 20,
                )
                .unwrap();
                let compatible = run(
                    &source,
                    EXACTEXTRACT_RASTERIO_POLICY,
                    strategy,
                    &mut cache,
                    &cancel,
                    64 << 20,
                )
                .unwrap();
                let after = run(
                    &source,
                    EXACTEXTRACT_POLICY,
                    strategy,
                    &mut cache,
                    &cancel,
                    64 << 20,
                )
                .unwrap();
                assert_eq!(before.values[3].to_bits(), 0_f64.to_bits());
                assert_eq!(compatible.values[3].to_bits(), (-0_f64).to_bits());
                assert_eq!(compatible.values[4].to_bits(), (-0_f64).to_bits());
                assert_eq!(before.values, after.values);
                assert_eq!(source.reads.get(), 1);
                assert_eq!(source.normalized_reads.get(), 1);
                assert_eq!(after.metrics["decoded_cache_hits"], 1);
            }
        }
    }
    #[test]
    fn masks_nan_nodata_and_dtype_cast_follow_unscaled_rasterio() {
        for kind in [RawScalarType::Float32, RawScalarType::Float64] {
            for strategy in ["feature-sequential", "raster-sequential"] {
                let source = Source::new(
                    kind,
                    vec![-0., f64::NAN, -9999., 7., -3.],
                    vec![255, 255, 0, 255, 255],
                );
                let result = run(
                    &source,
                    EXACTEXTRACT_RASTERIO_POLICY,
                    strategy,
                    &mut TileCache::default(),
                    &AtomicBool::new(false),
                    64 << 20,
                )
                .unwrap();
                assert_eq!(result.values, vec![4., 3., 4. / 3., -3., 7.]);
                assert_eq!(result.defined, vec![1; 5]);
            }
        }
        let mut source = Source::new(RawScalarType::Float32, vec![0.1], vec![255]);
        source.raw.bands[0].nodata_f64_bits = Some(0.1_f64.to_bits());
        source.metadata.bands[0].nodata = Some(0.1);
        let result = run(
            &source,
            EXACTEXTRACT_RASTERIO_POLICY,
            "raster-sequential",
            &mut TileCache::default(),
            &AtomicBool::new(false),
            64 << 20,
        )
        .unwrap();
        assert_eq!(result.values[1], 0.0);
        assert_eq!(result.defined[2..], [0, 0, 0]);
    }
    #[test]
    fn eligibility_and_raw_budget_fail_before_pixels() {
        for mode in 0..4 {
            let mut source = Source::new(RawScalarType::Float32, vec![1.], vec![255]);
            match mode {
                0 => source.raw.bands[0].scale_f64_bits = 2_f64.to_bits(),
                1 => source.metadata.bands[0].offset = 1.,
                2 => source.raw.bands[0].scalar_type = RawScalarType::UInt16,
                _ => source.metadata.bands[0].nodata = None,
            }
            assert!(
                run(
                    &source,
                    EXACTEXTRACT_RASTERIO_POLICY,
                    "raster-sequential",
                    &mut TileCache::default(),
                    &AtomicBool::new(false),
                    64 << 20
                )
                .is_err()
            );
            assert_eq!(source.reads.get(), 0);
        }
        let source = Source::new(RawScalarType::Float32, vec![1.], vec![255]);
        assert!(
            run(
                &source,
                EXACTEXTRACT_RASTERIO_POLICY,
                "raster-sequential",
                &mut TileCache::default(),
                &AtomicBool::new(false),
                4096
            )
            .is_err()
        );
        assert_eq!(source.reads.get(), 0);
    }
    #[test]
    fn unsupported_masks_and_contradictory_raw_masks_fail_closed() {
        // The pinned natural RasterView can include explicit masked finite
        // values. Reject those source contracts rather than copying wrong sums.
        for flags in [0, 2, 4, 6, 9, 16] {
            let mut source = Source::new(RawScalarType::Float32, vec![10000.], vec![0]);
            source.raw.bands[0].mask_flags = flags;
            let error = run(
                &source,
                EXACTEXTRACT_RASTERIO_POLICY,
                "feature-sequential",
                &mut TileCache::default(),
                &AtomicBool::new(false),
                64 << 20,
            )
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("all-valid or NoData-derived masks")
            );
            assert_eq!(source.reads.get(), 0);
        }
        for flags in [1, 8] {
            let mut source = Source::new(RawScalarType::Float32, vec![10000.], vec![0]);
            source.raw.bands[0].mask_flags = flags;
            assert!(
                run(
                    &source,
                    EXACTEXTRACT_RASTERIO_POLICY,
                    "raster-sequential",
                    &mut TileCache::default(),
                    &AtomicBool::new(false),
                    64 << 20
                )
                .is_err()
            );
            assert_eq!(source.reads.get(), 1);
        }
    }
    #[test]
    fn invalid_infinity_malformed_read_and_cancel_discard_complete_output() {
        for mode in 0..3 {
            let mut source = Source::new(RawScalarType::Float64, vec![1., 2.], vec![255, 255]);
            match mode {
                0 => source.values[1] = f64::INFINITY,
                1 => source.malformed = true,
                _ => source.cancel_after_read = true,
            }
            let cancel = AtomicBool::new(false);
            let mut cache = TileCache::default();
            cache.set_limit(1 << 20).unwrap();
            assert!(
                run(
                    &source,
                    EXACTEXTRACT_RASTERIO_POLICY,
                    "raster-sequential",
                    &mut cache,
                    &cancel,
                    64 << 20
                )
                .is_err()
            );
            assert_eq!(source.reads.get(), 1);
            assert!(cache.is_empty());
        }
    }
}
