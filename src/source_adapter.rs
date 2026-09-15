//! Ordinary bounded source opening. Location/auth never become analytical keys.
use super::*;
use crate::source::{
    RawBandMetadata, RawBandWindow, RawRasterMetadata, RawScalarType, RawWindow, SourceFormat,
    SourceSpec, VerificationPolicy,
};
use gdal::Metadata;
use serde_json::{Value, json};

struct SourceAdapter {
    // Dataset closes before transport registration is removed (field drop order).
    dataset: Dataset,
    remote: Option<source_vsi::Registration>,
    local: Option<(PathBuf, String)>,
    metadata: RasterMetadata,
    raw_metadata: Option<RawRasterMetadata>,
    mapping: Vec<usize>,
    descriptor: Option<Value>,
    interpretation: Value,
    verified_bytes: u64,
    verification_ms: f64,
    format_probe_bytes: u64,
    decode_band_factor: usize,
    mask_blocks: Vec<(usize, usize)>,
    decoder_cache_flushes: std::cell::Cell<u64>,
    decoder_cache_reused_chunks: std::cell::Cell<u64>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn raw_metadata(
    dataset: &Dataset,
    metadata: &RasterMetadata,
    mapping: &[usize],
    pixel_convention: &str,
    overview: Option<usize>,
) -> Result<RawRasterMetadata> {
    let bands = mapping
        .iter()
        .zip(&metadata.bands)
        .map(|(&index, info)| {
            let band = dataset.rasterband(index + 1)?;
            let flags = band.mask_flags()?;
            let description = band.description()?;
            ensure!(description.len() <= 1024, "band description exceeds budget");
            Ok(RawBandMetadata {
                scalar_type: RawScalarType::from_gdal_name(&info.data_type)?,
                nodata_f64_bits: info.nodata.map(f64::to_bits),
                scale_f64_bits: info.scale.to_bits(),
                offset_f64_bits: info.offset.to_bits(),
                unit: info.unit.clone(),
                mask_flags: u32::from(flags.is_all_valid())
                    | (u32::from(flags.is_per_dataset()) << 1)
                    | (u32::from(flags.is_alpha()) << 2)
                    | (u32::from(flags.is_nodata()) << 3),
                original_band_index: index,
                description,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RawRasterMetadata {
        bands,
        pixel_convention: pixel_convention.into(),
        source_band_count: dataset.raster_count(),
        source_overview: overview,
    })
}
/// Open once and retain this handle for a query group. Explicit Verify identity
/// may scan once here; verify_immutable and window reads never hash whole files.
pub fn open_source(spec: &SourceSpec, cancel: &AtomicBool) -> Result<Box<dyn WindowSource>> {
    open_source_with_band_limit(spec, cancel, 64)
}
/// Conversion may inspect an entire supported dataset. Reads remain grouped
/// into at most twenty bands and retain the same decoder/transport admission.
pub fn open_source_for_compile(
    spec: &SourceSpec,
    cancel: &AtomicBool,
) -> Result<Box<dyn WindowSource>> {
    ensure!(
        spec.format == SourceFormat::Geotiff,
        "lossless compilation currently requires a supported GeoTIFF/COG source"
    );
    open_source_with_band_limit(spec, cancel, 64)
}
fn open_source_with_band_limit(
    spec: &SourceSpec,
    cancel: &AtomicBool,
    max_bands: usize,
) -> Result<Box<dyn WindowSource>> {
    check_cancel(cancel)?;
    crate::source::validate_source_spec(spec)?;
    if let Some(ids) = &spec.bands {
        ensure!(
            ids.len() <= max_bands,
            "source band mapping must contain1..{max_bands} distinct indices"
        );
    }
    let is_remote = spec.location.starts_with("https://") || spec.location.starts_with("http://");
    let suffix_path = if is_remote {
        reqwest::Url::parse(&spec.location)
            .ok()
            .map(|url| url.path().to_owned())
    } else {
        Some(spec.location.clone())
    };
    let skv_suffix = suffix_path
        .as_deref()
        .and_then(|path| Path::new(path).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("skv"));
    let tiff_suffix = suffix_path
        .as_deref()
        .and_then(|path| Path::new(path).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("tif") || extension.eq_ignore_ascii_case("tiff")
        });
    // A suffix is only a routing hint; the SKV reader validates magic/version.
    if spec.format == SourceFormat::Skv || (spec.format == SourceFormat::Geotiff && skv_suffix) {
        return Ok(Box::new(crate::skv::SkvSource::open(spec, cancel)?));
    }
    let mut verified_bytes = 0;
    let mut format_probe_bytes = 0;
    let verification_started = std::time::Instant::now();
    let (remote, local, source_path, base_id) = if is_remote {
        ensure!(
            spec.format == SourceFormat::Geotiff,
            "remote NetCDF transport is not yet supported"
        );
        let mut source = RangeSource::register_configured(
            &spec.location,
            RemoteLimits {
                max_requests: spec.http.max_requests,
                max_download_bytes: spec.http.max_download_bytes,
                max_range_bytes: spec.http.max_range_bytes,
                timeout_seconds: spec.http.timeout_seconds,
            },
            Arc::new(AtomicBool::new(false)),
            Some(&spec.http),
        )?;
        check_cancel(cancel)?;
        // Unknown suffixes need a bounded magic probe. Reuse this registration
        // for either reader, so detection never adds another metadata HEAD.
        // The actual small range request(s) remain visible in transport metrics.
        let probe_length = if tiff_suffix { 0 } else { source.length.min(8) };
        let mut prefix = Vec::with_capacity(probe_length as usize);
        while prefix.len() < probe_length as usize {
            let count = (probe_length - prefix.len() as u64).min(spec.http.max_range_bytes);
            prefix.extend(source.read_range_cancellable(prefix.len() as u64, count, cancel)?);
        }
        format_probe_bytes = probe_length;
        if prefix.as_slice() == crate::skv::MAGIC {
            return Ok(Box::new(crate::skv::SkvSource::open_registered(
                spec, cancel, source,
            )?));
        }
        if let Some(identity) = &spec.identity {
            ensure!(
                source.length == identity.byte_length,
                "remote content length does not match identity"
            );
            if let Some(etag) = &identity.etag {
                ensure!(
                    &source.etag == etag,
                    "remote validator does not match manifest"
                );
            }
            if identity.policy == VerificationPolicy::TrustedManifest {
                ensure!(
                    identity.etag.is_some(),
                    "trusted remote manifest requires its object-scoped ETag"
                );
            } else {
                ensure!(
                    source.length <= spec.http.max_download_bytes,
                    "full identity verification exceeds explicit download budget"
                );
                let mut hash = Sha256::new();
                let mut offset = 0;
                while offset < source.length {
                    check_cancel(cancel)?;
                    let n = (source.length - offset).min(spec.http.max_range_bytes);
                    hash.update(source.read_range(offset, n)?);
                    offset += n;
                }
                ensure!(
                    format!("{:x}", hash.finalize()) == identity.sha256.to_ascii_lowercase(),
                    "remote content digest does not match identity"
                );
                verified_bytes = source.length;
            }
        }
        let id = source.source_id.clone();
        let registration = source_vsi::Registration::new(source)?;
        let path = registration.path.clone();
        (Some(registration), None, path, id)
    } else {
        ensure!(
            !spec.location.contains("://") && !spec.location.starts_with("/vsi"),
            "unsupported source transport"
        );
        let path = fs::canonicalize(&spec.location).context("source path is unavailable")?;
        let sig = fingerprint(&path)?;
        if spec.format == SourceFormat::Geotiff && !tiff_suffix {
            let length = fs::metadata(&path)?.len().min(8) as usize;
            let mut prefix = vec![0; length];
            fs::File::open(&path)?
                .read_exact(&mut prefix)
                .context("cannot read source format prefix")?;
            check_cancel(cancel)?;
            format_probe_bytes = length as u64;
            if prefix.as_slice() == crate::skv::MAGIC {
                return Ok(Box::new(crate::skv::SkvSource::open(spec, cancel)?));
            }
        }
        if let Some(identity) = &spec.identity {
            for suffix in [".msk", ".aux.xml"] {
                ensure!(
                    !PathBuf::from(format!("{}{suffix}", path.to_string_lossy())).exists(),
                    "portable source identity requires embedded metadata/masks; external sidecars are unsupported"
                );
            }
            ensure!(
                fs::metadata(&path)?.len() == identity.byte_length,
                "local content length does not match identity"
            );
            if identity.policy == VerificationPolicy::Verify {
                let mut file = fs::File::open(&path)?;
                let mut hash = Sha256::new();
                // A later verification branch must not reserve 64 KiB on every
                // source-open frame, including SKV dispatch into its decoder.
                // This bounded scratch is covered by the reader reservation.
                let mut buffer = vec![0u8; 65536];
                loop {
                    check_cancel(cancel)?;
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buffer[..n]);
                    verified_bytes += n as u64;
                }
                ensure!(
                    format!("{:x}", hash.finalize()) == identity.sha256.to_ascii_lowercase(),
                    "local content digest does not match identity"
                );
                ensure!(
                    fingerprint(&path)? == sig,
                    "source changed during verification"
                );
            }
        }
        let text = path
            .to_str()
            .context("source path must be UTF-8")?
            .to_owned();
        (None, Some((path, sig.clone())), text, sig)
    };
    let verification_ms = verification_started.elapsed().as_secs_f64() * 1000.;
    if spec.overview.is_some() && local.is_some() {
        for suffix in [".ovr", ".msk.ovr", ".aux"] {
            ensure!(
                !PathBuf::from(format!("{source_path}{suffix}")).exists(),
                "explicit overview views currently require internal TIFF overviews"
            );
        }
    }
    let _operation = remote.as_ref().map(|r| r.operation(cancel)).transpose()?;
    let _options = if remote.is_some() {
        Some(GdalOptions::remote()?)
    } else {
        None
    };
    // Thread-local settings are restored on scope exit. No nested decoder pool.
    let prior_threads = gdal::config::get_thread_local_config_option("GDAL_NUM_THREADS", "")?;
    gdal::config::set_thread_local_config_option("GDAL_NUM_THREADS", "1")?;
    let mut overview_parent = None;
    let opened = (|| {
        if spec.format == SourceFormat::Geotiff {
            ensure!(
                spec.variable.is_none(),
                "GeoTIFF source does not accept a variable selector"
            );
            if let Some(level) = spec.overview {
                // GDAL's overview proxy does not inherit optional band scale,
                // offset, unit or description. Capture the parent interpretation
                // before closing that metadata-only handle. This extra open uses
                // the same bounded transport registration and is fully metered.
                let parent = open_gtiff(&source_path)?;
                let parent_mapping = spec
                    .bands
                    .clone()
                    .unwrap_or_else(|| (0..parent.raster_count()).collect());
                let parent_metadata = metadata_selected_with_crs_limit(
                    &parent,
                    base_id.clone(),
                    &parent_mapping,
                    spec.crs.as_deref(),
                    max_bands,
                )?;
                let parent_raw = raw_metadata(
                    &parent,
                    &parent_metadata,
                    &parent_mapping,
                    &parent
                        .metadata_item("AREA_OR_POINT", "")
                        .unwrap_or_else(|| "Area".into()),
                    None,
                )?;
                overview_parent = Some((parent_metadata, parent_raw));
                drop(parent);
                // GDAL's existing-overview dataset exposes the original reduced
                // pixels and their affine. `only` forbids further sub-levels.
                // No RasterIO size conversion or overview generation is used.
                let option = format!("OVERVIEW_LEVEL={level}only");
                Dataset::open_ex(
                    &source_path,
                    DatasetOptions {
                        open_flags: GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_READONLY,
                        allowed_drivers: Some(&["GTiff"]),
                        open_options: Some(&[option.as_str()]),
                        ..Default::default()
                    },
                )
                .map_err(|_| anyhow!("cannot open explicitly selected TIFF overview"))
            } else {
                open_gtiff(&source_path)
            }
        } else {
            let variable = spec
                .variable
                .as_ref()
                .context("NetCDF source requires explicit variable")?;
            ensure!(
                !variable.is_empty()
                    && variable.len() <= 256
                    && variable
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "unsupported NetCDF variable selector"
            );
            ensure!(
                !source_path.contains('"'),
                "NetCDF path contains unsupported quoting"
            );
            Dataset::open_ex(
                format!("NETCDF:\"{source_path}\":{variable}"),
                DatasetOptions {
                    open_flags: GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_READONLY,
                    allowed_drivers: Some(&["netCDF"]),
                    ..Default::default()
                },
            )
            .map_err(|_| anyhow!("cannot open selected NetCDF variable"))
        }
    })();
    if prior_threads.is_empty() {
        gdal::config::clear_thread_local_config_option("GDAL_NUM_THREADS")?;
    } else {
        gdal::config::set_thread_local_config_option("GDAL_NUM_THREADS", &prior_threads)?;
    }
    if let Some(r) = &remote {
        r.check_error()?;
    }
    let dataset = opened?;
    ensure!(
        spec.bands.is_some() || dataset.raster_count() <= max_bands,
        "select1..{max_bands} native bands explicitly for this source slice"
    );
    let mapping = spec
        .bands
        .clone()
        .unwrap_or_else(|| (0..dataset.raster_count()).collect());
    ensure!(
        !mapping.is_empty() && mapping.len() <= max_bands,
        "select1..{max_bands} native bands explicitly for this source slice"
    );
    let mut metadata = metadata_selected_with_crs_limit(
        &dataset,
        base_id.clone(),
        &mapping,
        spec.crs.as_deref(),
        max_bands,
    )?;
    if let Some((parent, _)) = &overview_parent {
        ensure!(
            parent.bands.len() == metadata.bands.len(),
            "overview band mapping differs from parent"
        );
        for (exposed, (&index, info)) in mapping.iter().zip(&mut metadata.bands).enumerate() {
            let band = dataset.rasterband(index + 1)?;
            let original = &parent.bands[exposed];
            ensure!(
                info.data_type == original.data_type,
                "overview scalar type differs from parent"
            );
            // Explicit conflicting interpretation is never replaced. Missing
            // attributes inherit the same source band's declared interpretation.
            ensure!(
                band.scale().is_none_or(|v| v == original.scale)
                    && band.offset().is_none_or(|v| v == original.offset),
                "overview has conflicting scale or offset"
            );
            ensure!(
                info.nodata.is_none()
                    || info.nodata.map(f64::to_bits) == original.nodata.map(f64::to_bits),
                "overview has conflicting NoData"
            );
            ensure!(
                info.unit.is_none() || info.unit == original.unit,
                "overview has conflicting unit"
            );
            info.nodata = original.nodata;
            info.scale = original.scale;
            info.offset = original.offset;
            info.unit = original.unit.clone();
        }
    }
    let coordinates = if spec.format == SourceFormat::Netcdf {
        ensure!(
            dataset
                .metadata_domain("GEOLOCATION")
                .is_none_or(|v| v.is_empty()),
            "curvilinear/geolocation-array NetCDF grids are unsupported"
        );
        ensure!(
            spec.longitude_shift == 0 || metadata.grid.crs == "EPSG:4326",
            "longitude representation shift requires EPSG:4326"
        );
        Some(verify_netcdf_coordinates(
            &source_path,
            spec.variable.as_deref().unwrap(),
            &metadata.grid,
            spec.longitude_shift,
            cancel,
        )?)
    } else {
        None
    };
    let mask_flags: Vec<_> = mapping
        .iter()
        .map(|&i| {
            dataset
                .rasterband(i + 1)
                .and_then(|b| b.mask_flags())
                .map(|m| {
                    json!([
                        m.is_all_valid(),
                        m.is_per_dataset(),
                        m.is_alpha(),
                        m.is_nodata()
                    ])
                })
        })
        .collect::<std::result::Result<_, _>>()?;
    let mask_blocks = mapping
        .iter()
        .map(|&i| {
            dataset
                .rasterband(i + 1)?
                .open_mask_band()
                .map(|b| b.block_size())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        mask_blocks.iter().all(|&(w, h)| w > 0 && h > 0),
        "invalid physical mask block dimensions"
    );
    let registration = dataset
        .metadata_item("AREA_OR_POINT", "")
        .unwrap_or_else(|| "Area".into());
    ensure!(
        registration.len() <= 1024,
        "pixel convention exceeds budget"
    );
    let mut raw_metadata = if spec.format == SourceFormat::Geotiff {
        Some(raw_metadata(
            &dataset,
            &metadata,
            &mapping,
            &registration,
            spec.overview,
        )?)
    } else {
        None
    };
    if let (Some(raw), Some((_, parent_raw))) = (&mut raw_metadata, &overview_parent) {
        for (band, parent) in raw.bands.iter_mut().zip(&parent_raw.bands) {
            if band.description.is_empty() {
                band.description = parent.description.clone();
            }
        }
    }
    let interleave = dataset
        .metadata_item("INTERLEAVE", "IMAGE_STRUCTURE")
        .unwrap_or_else(|| "unknown".into());
    let decode_band_factor = if spec.format == SourceFormat::Geotiff && interleave != "BAND" {
        dataset.raster_count()
    } else {
        1
    };
    let mut interpretation = json!({"grid":metadata.grid,"bands":metadata.bands,"band_mapping":mapping,"format":spec.format,"variable":spec.variable,"crs_assignment":spec.crs,"longitude_shift":spec.longitude_shift,"mask_flags":mask_flags,
        "registration":registration,"axis_order":"traditional_gis_x_y","coordinate_validation":coordinates,"numerical_contract":"native_grid_planar_v1","decoding":"finite_raw_mask_nodata_then_scale_offset_v1"});
    if let Some(level) = spec.overview {
        interpretation["source_overview"] = json!(level);
        interpretation["source_view_policy"] =
            json!("explicit_existing_overview_inherit_missing_band_metadata_v1");
    }
    let interpretation_sha256 = digest(&serde_json::to_vec(&interpretation)?);
    let descriptor=spec.identity.as_ref().map(|id|json!({"version":1,"content_sha256":id.sha256.to_ascii_lowercase(),"byte_length":id.byte_length,"interpretation_sha256":interpretation_sha256}));
    metadata.source_id = if let Some(id) = &descriptor {
        format!(
            "content-interpretation-sha256:{}",
            digest(&serde_json::to_vec(id)?)
        )
    } else {
        format!(
            "source-interpretation-sha256:{}",
            digest(&serde_json::to_vec(&json!([
                base_id,
                interpretation_sha256
            ]))?)
        )
    };
    drop(_operation);
    let result = SourceAdapter {
        dataset,
        remote,
        local,
        metadata,
        raw_metadata,
        mapping,
        descriptor,
        interpretation,
        verified_bytes,
        verification_ms,
        format_probe_bytes,
        decode_band_factor,
        mask_blocks,
        decoder_cache_flushes: std::cell::Cell::new(0),
        decoder_cache_reused_chunks: std::cell::Cell::new(0),
    };
    result.verify_immutable()?;
    check_cancel(cancel)?;
    Ok(Box::new(result))
}

/// Validate coordinate vectors themselves; a GDAL-fitted affine alone would
/// accept some nearly regular vectors and would silently change their cells.
fn verify_netcdf_coordinates(
    path: &str,
    variable: &str,
    grid: &Grid,
    longitude_shift: i32,
    cancel: &AtomicBool,
) -> Result<Value> {
    use std::ffi::CString;
    let md = Dataset::open_ex(
        path,
        DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_MULTIDIM_RASTER | GdalOpenFlags::GDAL_OF_READONLY,
            allowed_drivers: Some(&["netCDF"]),
            ..Default::default()
        },
    )?;
    struct Group(gdal_sys::GDALGroupH);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe { gdal_sys::GDALGroupRelease(self.0) }
        }
    }
    struct Array(gdal_sys::GDALMDArrayH);
    impl Drop for Array {
        fn drop(&mut self) {
            unsafe { gdal_sys::GDALMDArrayRelease(self.0) }
        }
    }
    struct Dims(*mut gdal_sys::GDALDimensionH, usize);
    impl Drop for Dims {
        fn drop(&mut self) {
            unsafe { gdal_sys::GDALReleaseDimensions(self.0, self.1) }
        }
    }
    struct Type(gdal_sys::GDALExtendedDataTypeH);
    impl Drop for Type {
        fn drop(&mut self) {
            unsafe { gdal_sys::GDALExtendedDataTypeRelease(self.0) }
        }
    }
    // Handles are checked before use and each owning C handle has one Drop.
    let group = Group(unsafe { gdal_sys::GDALDatasetGetRootGroup(md.c_dataset()) });
    ensure!(
        !group.0.is_null(),
        "NetCDF multidimensional metadata unavailable"
    );
    let name = CString::new(variable)?;
    let array = Array(unsafe {
        gdal_sys::GDALGroupOpenMDArray(group.0, name.as_ptr(), std::ptr::null_mut())
    });
    ensure!(
        !array.0.is_null(),
        "NetCDF variable unavailable for coordinate validation"
    );
    let mut n = 0;
    let dims = Dims(
        unsafe { gdal_sys::GDALMDArrayGetDimensions(array.0, &mut n) },
        n,
    );
    ensure!(
        !dims.0.is_null() && (2..=8).contains(&n),
        "unsupported NetCDF dimensions"
    );
    let kind =
        Type(unsafe { gdal_sys::GDALExtendedDataTypeCreate(gdal_sys::GDALDataType::GDT_Float64) });
    ensure!(!kind.0.is_null(), "cannot create coordinate datatype");
    let mut hash = Sha256::new();
    let mut coordinate_bytes = 0;
    for (axis, expected_size) in [(n - 1, grid.width), (n - 2, grid.height)] {
        let dim = unsafe { *dims.0.add(axis) };
        let size = unsafe { gdal_sys::GDALDimensionGetSize(dim) };
        ensure!(
            size == expected_size as u64 && size >= 2 && size <= 2_000_000,
            "NetCDF spatial coordinate shape/size unsupported"
        );
        let coordinates = Array(unsafe { gdal_sys::GDALDimensionGetIndexingVariable(dim) });
        ensure!(
            !coordinates.0.is_null()
                && unsafe { gdal_sys::GDALMDArrayGetDimensionCount(coordinates.0) } == 1,
            "NetCDF requires one-dimensional spatial coordinate vectors"
        );
        let read = |start: u64, output: &mut [f64]| -> Result<()> {
            let count = output.len();
            let p = output.as_mut_ptr();
            ensure!(
                unsafe {
                    gdal_sys::GDALMDArrayRead(
                        coordinates.0,
                        &start,
                        &count,
                        std::ptr::null(),
                        std::ptr::null(),
                        kind.0,
                        p.cast(),
                        p.cast(),
                        count * 8,
                    )
                } != 0,
                "cannot read NetCDF coordinates"
            );
            Ok(())
        };
        let mut endpoints = [0., 0.];
        read(0, &mut endpoints[..1])?;
        read(size - 1, &mut endpoints[1..])?;
        let x_axis = axis == n - 1;
        let increasing = endpoints[1] > endpoints[0];
        ensure!(
            !x_axis || increasing,
            "descending NetCDF x coordinates unsupported"
        );
        let mut values = vec![0.; 65536.min(size as usize)];
        for start in (0..size as usize).step_by(values.len()) {
            check_cancel(cancel)?;
            let count = values.len().min(size as usize - start);
            read(start as u64, &mut values[..count])?;
            for (j, &value) in values[..count].iter().enumerate() {
                let index = start + j;
                let exposed = if !x_axis && increasing {
                    size as usize - 1 - index
                } else {
                    index
                };
                let (origin, step) = if x_axis {
                    (grid.transform[0], grid.transform[1])
                } else {
                    (grid.transform[3], grid.transform[5])
                };
                let expected = origin + (exposed as f64 + 0.5) * step;
                let interpreted = if x_axis {
                    value + longitude_shift as f64
                } else {
                    value
                };
                let tolerance =
                    32. * f64::EPSILON * interpreted.abs().max(expected.abs()).max(step.abs());
                ensure!(
                    interpreted.is_finite() && (interpreted - expected).abs() <= tolerance,
                    "NetCDF coordinates do not match native affine; irregular grid or undeclared longitude translation"
                );
                hash.update(value.to_le_bytes());
            }
            coordinate_bytes += count * 8;
        }
    }
    Ok(
        json!({"coordinate_sha256":format!("{:x}",hash.finalize()),"coordinate_bytes_read":coordinate_bytes,"longitude_shift":longitude_shift,"policy":"cell_centres_match_native_affine_within32_epsilon_scaled_v1"}),
    )
}
impl WindowSource for SourceAdapter {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        self.raw_metadata.as_ref()
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, indices: &[usize]) -> Result<usize> {
        ensure!(self.raw_metadata.is_some(), "source lacks typed raw access");
        ensure!(
            !indices.is_empty()
                && indices.len() <= 20
                && indices
                    .iter()
                    .enumerate()
                    .all(|(i, b)| !indices[..i].contains(b)),
            "raw reads require 1..20 distinct bands per bounded group"
        );
        let base = self.read_buffer_bound(w, h, indices)?;
        // Existing normalized output (nine bytes/sample) bounds typed output
        // plus its independent byte mask. An additional aligned typed buffer
        // preserves scalar bits without writing through an unaligned u8 pointer.
        let rows = (65536 / w).max(1).min(h);
        base.checked_add(
            w.checked_mul(rows)
                .and_then(|n| n.checked_mul(8))
                .context("typed raw scratch overflow")?,
        )
        .context("typed raw buffer bound overflow")
    }
    fn read_raw_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(RawWindow, ReadMetrics)> {
        check_cancel(cancel)?;
        ensure!(
            self.raw_read_buffer_bound(w, h, indices)? <= max_bytes,
            "raw source window exceeds combined decoder/transport memory budget"
        );
        if let Some((path, sig)) = &self.local {
            ensure!(
                fingerprint(path)? == *sig,
                "source changed since registration"
            );
        }
        let _operation = self
            .remote
            .as_ref()
            .map(|r| r.operation(cancel))
            .transpose()?;
        let _options = if self.remote.is_some() {
            Some(GdalOptions::remote()?)
        } else {
            None
        };
        let result = read_raw_window_mapped(
            &self.dataset,
            &self.metadata,
            self.raw_metadata
                .as_ref()
                .context("source lacks typed raw access")?,
            &self.mapping,
            x,
            y,
            w,
            h,
            indices,
            cancel,
        );
        if let Some(remote) = &self.remote {
            remote.check_error()?;
        }
        if let Some((path, sig)) = &self.local {
            ensure!(fingerprint(path)? == *sig, "source changed during raw read");
        }
        check_cancel(cancel)?;
        if let Ok((_, metrics)) = &result {
            self.decoder_cache_flushes.set(
                self.decoder_cache_flushes
                    .get()
                    .saturating_add(metrics.decoder_cache_flushes as u64),
            );
            self.decoder_cache_reused_chunks.set(
                self.decoder_cache_reused_chunks
                    .get()
                    .saturating_add(metrics.decoder_cache_reused_chunks as u64),
            );
        }
        result
    }
    fn verify_immutable(&self) -> Result<()> {
        if let Some((path, sig)) = &self.local {
            ensure!(
                fingerprint(path)? == *sig,
                "source changed since registration"
            );
        }
        if let Some(remote) = &self.remote {
            remote.verify()?;
        }
        Ok(())
    }
    fn identity_descriptor(&self) -> Option<Value> {
        self.descriptor.clone()
    }
    fn diagnostics(&self) -> Value {
        json!({"kind":if self.remote.is_some(){"http_original"}else{"local_original"},"content_verification_bytes":self.verified_bytes,"content_verification_ms":self.verification_ms,"format_probe_bytes":self.format_probe_bytes,"identity_policy":if self.descriptor.is_some(){if self.verified_bytes>0{"verified_registration"}else{"trusted_manifest"}}else{"session_stat_or_object_validator"},"remote":self.remote.as_ref().map(|r|r.metrics()),"adapter_retained_capacity_bytes":self.retained_memory_bound(),"physical_bytes_known":self.remote.is_some(),"gdal_block_cache":"shared_only_for_identical_data_and_mask_block_footprints_then_flushed_on_transition_or_window_exit","decode_band_factor":self.decode_band_factor,"decoder_cache_flushes":self.decoder_cache_flushes.get(),"decoder_cache_reused_chunks":self.decoder_cache_reused_chunks.get()})
    }
    fn retained_memory_bound(&self) -> usize {
        crate::source::NATIVE_SOURCE_RETAINED_BYTES
    }
    fn access_layout(&self) -> Value {
        let mut layout = self.interpretation.clone();
        // Physical capacity identity belongs to execution planning, never the
        // portable mathematical/source interpretation digest.
        layout["physical_access"] = json!({"mask_block_sizes":self.mask_blocks,
            "decode_band_factor":self.decode_band_factor,
            "transport_scratch_bound_bytes":self.remote.as_ref().map_or(0,|r|r.range_bound()+65536)});
        layout
    }
    fn read_buffer_bound(&self, w: usize, h: usize, indices: &[usize]) -> Result<usize> {
        ensure!(
            !indices.is_empty() && indices.len() <= 20,
            "source reads require 1..20 bands per bounded group"
        );
        let base = selected_window_buffer_bound(&self.metadata, w, h, indices)?;
        let rows = (65536 / w).max(1).min(h);
        let mut decoded: usize = 0;
        for &i in indices {
            let b = self
                .metadata
                .bands
                .get(i)
                .context("source band index out of range")?;
            let (bw, bh) = b.block_size;
            ensure!(bw > 0 && bh > 0, "invalid physical source block dimensions");
            let bound = w
                .div_ceil(bw)
                .checked_add(1)
                .and_then(|x| {
                    h.min(rows)
                        .div_ceil(bh)
                        .checked_add(1)
                        .and_then(|y| x.checked_mul(y))
                })
                .and_then(|n| n.checked_mul(bw))
                .and_then(|n| n.checked_mul(bh))
                .and_then(|n| n.checked_mul(9))
                .and_then(|n| n.checked_mul(self.decode_band_factor))
                .context("decoder physical block bound overflow")?;
            decoded = if self.decode_band_factor == 1 {
                decoded
                    .checked_add(bound)
                    .context("selected decoder block bound overflow")?
            } else {
                decoded.max(bound)
            };
        }
        // Mask storage can have a different footprint from data storage. The
        // separate reservation is conservative even when a shared mask is reused.
        for &i in indices {
            let (bw, bh) = self.mask_blocks[i];
            let mask_bound = w
                .div_ceil(bw)
                .checked_add(1)
                .and_then(|x| {
                    h.min(rows)
                        .div_ceil(bh)
                        .checked_add(1)
                        .and_then(|y| x.checked_mul(y))
                })
                .and_then(|n| n.checked_mul(bw))
                .and_then(|n| n.checked_mul(bh))
                .context("decoder mask block bound overflow")?;
            decoded = decoded
                .checked_add(mask_bound)
                .context("decoder data/mask block bound overflow")?;
        }
        base.checked_add(decoded)
            .and_then(|n| {
                n.checked_add(self.remote.as_ref().map_or(0, |r| r.range_bound() + 65536))
            })
            .context("source adapter buffer overflow")
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        check_cancel(cancel)?;
        ensure!(
            self.read_buffer_bound(w, h, indices)? <= max_bytes,
            "source window exceeds combined decoder/transport memory budget"
        );
        // Local guards bracket every read. Remote GETs use If-Match; callers also
        // verify the object at request boundaries, including summary-only reads.
        if let Some((path, sig)) = &self.local {
            ensure!(
                fingerprint(path)? == *sig,
                "source changed since registration"
            );
        }
        let _operation = self
            .remote
            .as_ref()
            .map(|r| r.operation(cancel))
            .transpose()?;
        let _options = if self.remote.is_some() {
            Some(GdalOptions::remote()?)
        } else {
            None
        };
        let result = read_selected_window_mapped(
            &self.dataset,
            &self.metadata,
            x,
            y,
            w,
            h,
            indices,
            max_bytes,
            cancel,
            Some(&self.mapping),
        );
        if let Some(remote) = &self.remote {
            remote.check_error()?;
        }
        if let Some((path, sig)) = &self.local {
            ensure!(fingerprint(path)? == *sig, "source changed during read");
        }
        check_cancel(cancel)?;
        if let Ok((_, metrics)) = &result {
            self.decoder_cache_flushes.set(
                self.decoder_cache_flushes
                    .get()
                    .saturating_add(metrics.decoder_cache_flushes as u64),
            );
            self.decoder_cache_reused_chunks.set(
                self.decoder_cache_reused_chunks
                    .get()
                    .saturating_add(metrics.decoder_cache_reused_chunks as u64),
            );
        }
        result
    }
}

/// Read in each band's original scalar type. Same-type GDAL RasterIO does not
/// invoke the normalized f64 path, so invalid payloads and signed zeros survive.
#[allow(clippy::too_many_arguments)]
fn read_raw_window_mapped(
    dataset: &Dataset,
    metadata: &RasterMetadata,
    raw_metadata: &RawRasterMetadata,
    mapping: &[usize],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    indices: &[usize],
    cancel: &AtomicBool,
) -> Result<(RawWindow, ReadMetrics)> {
    check_cancel(cancel)?;
    ensure!(
        width > 0
            && height > 0
            && x.checked_add(width)
                .is_some_and(|n| n <= metadata.grid.width)
            && y.checked_add(height)
                .is_some_and(|n| n <= metadata.grid.height),
        "raw window lies outside raster"
    );
    let cells = width
        .checked_mul(height)
        .context("raw window size overflow")?;
    let chunk_rows = (65536 / width).max(1).min(height);
    let mut bands = indices
        .iter()
        .map(|&i| {
            let bytes = cells
                .checked_mul(raw_metadata.bands[i].scalar_type.byte_width())
                .context("raw band byte size overflow")?;
            Ok(RawBandWindow {
                samples_le: vec![0; bytes],
                mask: vec![0; cells],
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let data = indices
        .iter()
        .map(|&i| dataset.rasterband(mapping[i] + 1))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let masks = data
        .iter()
        .map(|band| band.open_mask_band())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let shapes = data
        .iter()
        .chain(&masks)
        .map(|band| band.block_size())
        .collect::<Vec<_>>();
    ensure!(
        shapes.iter().all(|&(w, h)| w > 0 && h > 0),
        "invalid raw decoder block dimensions"
    );
    struct CacheGuard<'a>(&'a Dataset);
    impl Drop for CacheGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                gdal_sys::GDALFlushCache(self.0.c_dataset());
            }
        }
    }
    let _guard = CacheGuard(dataset);
    let mut metrics = ReadMetrics {
        decoder_cache_flushes: 1,
        ..ReadMetrics::default()
    };
    let mut prior = Vec::new();
    for row in (0..height).step_by(chunk_rows) {
        check_cancel(cancel)?;
        let rows = chunk_rows.min(height - row);
        let count = rows * width;
        let footprint = shapes
            .iter()
            .map(|&(bw, bh)| {
                (
                    x / bw,
                    (x + width - 1) / bw,
                    (y + row) / bh,
                    (y + row + rows - 1) / bh,
                )
            })
            .collect::<Vec<_>>();
        if !prior.is_empty() && prior != footprint {
            unsafe {
                gdal_sys::GDALFlushCache(dataset.c_dataset());
            }
            metrics.decoder_cache_flushes += 1;
        } else if !prior.is_empty() {
            metrics.decoder_cache_reused_chunks += 1;
        }
        for (selected, &index) in indices.iter().enumerate() {
            check_cancel(cancel)?;
            let kind = raw_metadata.bands[index].scalar_type;
            let begin = row * width;
            let output = &mut bands[selected];
            let started = std::time::Instant::now();
            macro_rules! read_typed {
                ($scalar:ty) => {{
                    let mut values = vec![0 as $scalar; count];
                    data[selected].read_into_slice::<$scalar>(
                        (x as isize, (y + row) as isize),
                        (width, rows),
                        (width, rows),
                        &mut values,
                        None,
                    )?;
                    check_cancel(cancel)?;
                    let bytes = &mut output.samples_le
                        [begin * kind.byte_width()..(begin + count) * kind.byte_width()];
                    for (i, (value, dest)) in values
                        .iter()
                        .zip(bytes.chunks_exact_mut(kind.byte_width()))
                        .enumerate()
                    {
                        if i % 4096 == 0 {
                            check_cancel(cancel)?;
                        }
                        dest.copy_from_slice(&value.to_le_bytes());
                    }
                }};
            }
            match kind {
                RawScalarType::Byte => read_typed!(u8),
                RawScalarType::Int8 => read_typed!(i8),
                RawScalarType::UInt16 => read_typed!(u16),
                RawScalarType::Int16 => read_typed!(i16),
                RawScalarType::UInt32 => read_typed!(u32),
                RawScalarType::Int32 => read_typed!(i32),
                RawScalarType::Float32 => read_typed!(f32),
                RawScalarType::Float64 => read_typed!(f64),
            }
            masks[selected].read_into_slice::<u8>(
                (x as isize, (y + row) as isize),
                (width, rows),
                (width, rows),
                &mut output.mask[begin..begin + count],
                None,
            )?;
            check_cancel(cancel)?;
            metrics.read_decode_ms += started.elapsed().as_secs_f64() * 1000.;
            metrics.raster_io_calls += 2;
        }
        prior = footprint;
    }
    Ok((
        RawWindow {
            width,
            height,
            bands,
        },
        metrics,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_cache_accounts_eviction_and_never_hides_changed_validator() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/bytes", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            for (i, connection) in listener.incoming().take(5).enumerate() {
                let mut connection = connection.unwrap();
                let mut request = vec![];
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    connection.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let etag = if i == 4 { "\"changed\"" } else { "\"fixed\"" };
                if request.starts_with("HEAD ") {
                    write!(connection,"HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Length:32\r\nAccept-Ranges: bytes\r\nConnection:close\r\n\r\n").unwrap();
                } else {
                    let range = request
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .unwrap()
                        .split('=')
                        .nth(1)
                        .unwrap();
                    let (a, b) = range.split_once('-').unwrap();
                    let (a, b) = (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap());
                    write!(connection,"HTTP/1.1 206 Partial Content\r\nETag: {etag}\r\nContent-Length:{}\r\nContent-Range:bytes {a}-{b}/32\r\nConnection:close\r\n\r\n",b-a+1).unwrap();
                    connection.write_all(&vec![a as u8; b - a + 1]).unwrap();
                }
            }
        });
        let options = crate::source::HttpOptions {
            allow_http: true,
            cache_bytes: 256,
            ..Default::default()
        };
        let mut source = RangeSource::register_configured(
            &url,
            RemoteLimits::default(),
            Arc::new(AtomicBool::new(false)),
            Some(&options),
        )
        .unwrap();
        assert_eq!(source.read_range(0, 8).unwrap(), vec![0; 8]);
        assert_eq!(source.read_range(0, 8).unwrap(), vec![0; 8]);
        assert_eq!(source.read_range(2, 3).unwrap(), vec![0; 3]);
        assert_eq!(source.read_range(8, 8).unwrap(), vec![8; 8]);
        assert_eq!(source.read_range(0, 8).unwrap(), vec![0; 8]);
        assert_eq!(source.metrics.cache_hits, 2);
        assert_eq!(source.metrics.cache_contained_hits, 1);
        assert_eq!(source.metrics.cache_evictions, 2);
        assert!(source.metrics.cache_peak_bytes <= 256);
        assert_eq!(source.metrics.get_requests, 3);
        assert!(source.verify_remote().is_err());
        assert_eq!(source.metrics.head_requests, 2);
        assert_eq!(source.metrics.failed_requests, 1);
        assert_eq!(source.metrics.cache_resident_bytes, 0);
        assert!(source.read_range(0, 8).is_err());
        worker.join().unwrap();
    }
}
