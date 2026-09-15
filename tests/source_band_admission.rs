use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    model::{Band, Grid, Raster, check_cancel},
    source::{BandMetadata, RasterMetadata, ReadMetrics, SourceSpec, WindowSource},
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{cell::RefCell, sync::atomic::AtomicBool};

const READ_LIMIT: usize = 64 << 20;

struct CapacitySource {
    metadata: RasterMetadata,
    full_group_cap: usize,
    bound_error: bool,
    reads: RefCell<Vec<(usize, usize, usize, usize, Vec<usize>)>>,
}
impl CapacitySource {
    fn new(full_group_cap: usize) -> Self {
        Self {
            metadata: RasterMetadata {
                grid: Grid {
                    width: 300,
                    height: 4,
                    transform: [0., 1., 0., 4., 0., -1.],
                    crs: "LOCAL".into(),
                },
                source_id: format!("capacity-source-{full_group_cap}"),
                bands: (0..40)
                    .map(|band| BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: Some(format!("band-{band}")),
                        block_size: (256, 256),
                    })
                    .collect(),
            },
            full_group_cap,
            bound_error: false,
            reads: RefCell::new(Vec::new()),
        }
    }
    fn sample(band: usize, x: usize, y: usize) -> (f64, bool) {
        (
            (band + 1) as f64 * 100. + x as f64 * 0.25 - y as f64 * 0.5,
            (x + 3 * y + band) % 17 != 0,
        )
    }
}
impl WindowSource for CapacitySource {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_buffer_bound(&self, width: usize, _height: usize, bands: &[usize]) -> Result<usize> {
        ensure!(!self.bound_error, "invalid source-bound sentinel");
        ensure!(!bands.is_empty() && bands.len() <= 20, "invalid read group");
        if self.full_group_cap == 0 {
            return Ok(READ_LIMIT + 1);
        }
        let cap = if width < 256 { 20 } else { self.full_group_cap };
        Ok(READ_LIMIT * bands.len() / cap)
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        self.reads
            .borrow_mut()
            .push((x, y, width, height, indices.to_vec()));
        check_cancel(cancel)?;
        ensure!(max_bytes == READ_LIMIT, "streaming changed its read budget");
        ensure!(
            self.read_buffer_bound(width, height, indices)? <= max_bytes,
            "unadmitted physical read"
        );
        let mut grid = self.metadata.grid.clone();
        grid.width = width;
        grid.height = height;
        grid.transform[0] += x as f64;
        grid.transform[3] -= y as f64;
        let bands = indices
            .iter()
            .map(|&band| {
                let samples = (y..y + height)
                    .flat_map(|row| (x..x + width).map(move |col| Self::sample(band, col, row)))
                    .collect::<Vec<_>>();
                Band {
                    values: samples.iter().map(|sample| sample.0).collect(),
                    valid: samples.iter().map(|sample| sample.1).collect(),
                    unit: self.metadata.bands[band].unit.clone(),
                }
            })
            .collect();
        Ok((
            Raster {
                grid,
                bands,
                source_id: self.metadata.source_id.clone(),
            },
            ReadMetrics {
                raster_io_calls: indices.len(),
                ..Default::default()
            },
        ))
    }
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]})
}
fn options() -> Options {
    Options {
        // A permutation exercises mapping across variable group offsets.
        bands: (0..40).map(|i| i * 17 % 40).collect(),
        statistics: Some(
            ["sum", "support", "mean", "min", "max"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Default::default()
    }
}
fn measure(source: &dyn WindowSource, geometry: &Value, options: &Options) -> Result<Value> {
    streaming::measure_cached_source(
        source,
        geometry,
        &source.metadata().grid.crs,
        options,
        &AtomicBool::new(false),
        &mut TileCache::default(),
        0.,
    )
}

#[test]
fn adaptive_groups_preserve_order_dyadic_answers_edges_and_cache() {
    let source = CapacitySource::new(17);
    let control = CapacitySource::new(20);
    let geometry = rectangle(0.125, 0.125, 299.875, 3.875);
    let options = options();
    let mut cache = TileCache::default();
    cache.set_limit(1 << 20).unwrap();
    let actual = streaming::measure_cached_source(
        &source,
        &geometry,
        "LOCAL",
        &options,
        &AtomicBool::new(false),
        &mut cache,
        0.,
    )
    .unwrap();
    let expected = measure(&control, &geometry, &options).unwrap();
    assert_eq!(actual["bands"], expected["bands"]);
    assert_eq!(actual["streaming"]["tile_allocation_budget"], READ_LIMIT);
    assert_eq!(actual["streaming"]["tiles_selected"], 2);
    let reads = source.reads.borrow();
    assert_eq!(
        reads.iter().map(|read| read.4.len()).collect::<Vec<_>>(),
        [17, 17, 6, 20, 20]
    );
    for x in [0, 256] {
        assert_eq!(
            reads
                .iter()
                .filter(|read| read.0 == x)
                .flat_map(|read| read.4.iter().copied())
                .collect::<Vec<_>>(),
            options.bands,
            "each occupied tile reads every requested band exactly once and in order"
        );
    }
    drop(reads);
    for (row, &band) in actual["bands"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&options.bands)
    {
        let (mut sum, mut support) = (0., 0.);
        let (mut minimum, mut maximum) = (f64::INFINITY, f64::NEG_INFINITY);
        for y in 0..4 {
            for x in 0..300 {
                let (value, valid) = CapacitySource::sample(band, x, y);
                if !valid {
                    continue;
                }
                // Native-grid fractions are dyadic, so these small totals are exact.
                let fraction = if x == 0 || x == 299 { 0.875 } else { 1. }
                    * if y == 0 || y == 3 { 0.875 } else { 1. };
                sum += fraction * value;
                support += fraction;
                minimum = minimum.min(value);
                maximum = maximum.max(value);
            }
        }
        assert_eq!(row["band"], band);
        assert_eq!(row["fractional_sum"], sum);
        assert_eq!(row["covered_cell_equivalents"], support);
        assert_eq!(row["coverage_weighted_mean"], sum / support);
        assert_eq!(row["min"], minimum);
        assert_eq!(row["max"], maximum);
    }
    let repeated = streaming::measure_cached_source(
        &source,
        &geometry,
        "LOCAL",
        &options,
        &AtomicBool::new(false),
        &mut cache,
        0.,
    )
    .unwrap();
    assert_eq!(repeated["bands"], actual["bands"]);
    assert_eq!(source.reads.borrow().len(), 5);
    assert_eq!(repeated["streaming"]["cache_hits"], 5);
}

#[test]
fn thin_geometry_keeps_the_same_numerical_result_under_smaller_groups() {
    let geometry = rectangle(0.125, 1.75, 299.875, 1.75 + 1e-10);
    let actual = measure(&CapacitySource::new(3), &geometry, &options()).unwrap();
    let expected = measure(&CapacitySource::new(20), &geometry, &options()).unwrap();
    assert_eq!(actual["bands"], expected["bands"]);
    assert!(
        actual["bands"][0]["covered_cell_equivalents"]
            .as_f64()
            .unwrap()
            > 0.
    );
}

#[test]
fn one_band_floor_and_invalid_bounds_fail_before_any_source_read() {
    let geometry = rectangle(0., 0., 300., 4.);
    let too_large = CapacitySource::new(0);
    let error = measure(&too_large, &geometry, &options()).unwrap_err();
    assert!(error.to_string().contains("even for one band"));
    assert!(too_large.reads.borrow().is_empty());
    let mut invalid = CapacitySource::new(17);
    invalid.bound_error = true;
    let error = measure(&invalid, &geometry, &options()).unwrap_err();
    assert_eq!(error.to_string(), "invalid source-bound sentinel");
    assert!(invalid.reads.borrow().is_empty());
}

#[test]
fn at_most_twenty_and_weighted_admission_remain_unchanged() {
    let geometry = rectangle(0., 0., 300., 4.);
    for weighted in [false, true] {
        let source = CapacitySource::new(17);
        let options = if weighted {
            Options {
                bands: (0..20).collect(),
                weight_band: Some(19),
                statistics: Some(vec!["weighted_sum".into()]),
                ..Default::default()
            }
        } else {
            Options {
                bands: (0..20).collect(),
                ..options()
            }
        };
        let error = measure(&source, &geometry, &options).unwrap_err();
        assert_eq!(
            error.to_string(),
            "source read exceeds streaming memory budget"
        );
        assert!(source.reads.borrow().is_empty());
    }
}

#[test]
fn real_band_tiled_forty_band_source_fits_smaller_groups() {
    use gdal::{
        DriverManager,
        raster::{Buffer, RasterCreationOptions},
        spatial_ref::SpatialRef,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("forty-band-tiled.tif");
    let creation = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=256",
        "BLOCKYSIZE=256",
        "INTERLEAVE=BAND",
        "COMPRESS=DEFLATE",
        "NUM_THREADS=1",
    ]);
    let mut dataset = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type_with_options::<f32, _>(&path, 256, 256, 40, &creation)
        .unwrap();
    dataset
        .set_geo_transform(&[0., 1., 0., 256., 0., -1.])
        .unwrap();
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for band in 1..=40 {
        dataset
            .rasterband(band)
            .unwrap()
            .write(
                (0, 0),
                (256, 256),
                &mut Buffer::new((256, 256), vec![band as f32; 256 * 256]),
            )
            .unwrap();
    }
    dataset.flush_cache().unwrap();
    drop(dataset);
    let spec: SourceSpec = serde_json::from_value(json!({"location":path})).unwrap();
    let source = raster_engine::io::open_source(&spec, &AtomicBool::new(false)).unwrap();
    assert!(
        source
            .read_buffer_bound(256, 256, &(0..20).collect::<Vec<_>>())
            .unwrap()
            > READ_LIMIT
    );
    assert!(source.read_buffer_bound(256, 256, &[0]).unwrap() <= READ_LIMIT);
    let options = options();
    let result = measure(source.as_ref(), &rectangle(0., 0., 256., 256.), &options).unwrap();
    for (row, &band) in result["bands"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&options.bands)
    {
        assert_eq!(row["band"], band);
        assert_eq!(
            row["fractional_sum"].as_f64().unwrap(),
            ((band + 1) * 256 * 256) as f64
        );
        assert_eq!(row["covered_cell_equivalents"].as_f64().unwrap(), 65536.);
        assert_eq!(
            row["coverage_weighted_mean"].as_f64().unwrap(),
            (band + 1) as f64
        );
        assert_eq!(row["min"].as_f64().unwrap(), (band + 1) as f64);
        assert_eq!(row["max"].as_f64().unwrap(), (band + 1) as f64);
    }
    assert_eq!(result["streaming"]["tile_allocation_budget"], READ_LIMIT);
    assert!(result["streaming"]["tiles_read"].as_u64().unwrap() > 2);
}
