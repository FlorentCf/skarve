// `cargo run --example rust_skv -- <new-output-directory>`
// Generates its own tiny TIFF: no Python, downloaded data or credentials.
use anyhow::{Context, Result, ensure};
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{CarveOptions, Skarve, skv::CompileOptions};
use serde_json::json;
use std::path::PathBuf;

fn main() -> Result<()> {
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("Usage: rust_skv <new-output-directory>")?,
    );
    std::fs::create_dir(&directory).context("output directory must not already exist")?;
    let original = directory.join("example.tif");
    let prepared = directory.join("example.skv");
    {
        let mut dataset = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<f32, _>(&original, 4, 2, 2)?;
        dataset.set_geo_transform(&[0., 1., 0., 2., 0., -1.])?;
        dataset.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        for index in 1..=2 {
            let mut band = dataset.rasterband(index)?;
            let values: Vec<f32> = (1..=8).map(|v| (v * index) as f32).collect();
            band.write((0, 0), (4, 2), &mut Buffer::new((4, 2), values))?;
        }
        dataset.flush_cache()?;
    }
    let zone = json!({"type":"Polygon", "coordinates":[[[0.,0.],[4.,0.],[4.,2.],[0.,2.],[0.,0.]]]});
    let options = CarveOptions {
        metrics: ["sum", "support", "mean", "min", "max"]
            .map(str::to_owned)
            .to_vec(),
        ..Default::default()
    };
    let mut skarve = Skarve::new();
    let direct;
    let compilation;
    {
        let mut source = skarve.infuse(original.to_str().context("UTF-8 path required")?)?;
        direct = source.carve(&zone, &options)?;
        compilation = source.compile(
            prepared.to_str().context("UTF-8 path required")?,
            &CompileOptions {
                chunk_edge: 64,
                ..Default::default()
            },
        )?;
    } // Source is closed by Drop.
    std::fs::remove_file(&original)?; // Prove serving does not require the original.
    let single = {
        let mut source = skarve.infuse(prepared.to_str().context("UTF-8 path required")?)?;
        source.carve(&zone, &options)?
    };
    ensure!(
        single["bands"] == direct["bands"],
        "TIFF/SKV answers differ"
    );
    ensure!(
        single["bands"][0]["fractional_sum"] == 36.0,
        "unexpected first sum"
    );
    ensure!(
        single["bands"][1]["fractional_sum"] == 72.0,
        "unexpected second sum"
    );
    let job = json!({
        "zones":[{"id":"whole", "version":"1", "geometry":zone},
                 {"id":"repeat", "version":"1", "geometry":zone}],
        "slices":[{"id":"snapshot", "spec":{"location":prepared.to_str()}}],
        "crs":"EPSG:3857", "options":{"statistics":options.metrics},
        "budget":{"working_bytes":67108864, "geometry_bytes":8388608,
                  "tile_bytes":16777216, "output_bytes":1048576}
    });
    let mut rows = 0;
    let mut complete = false;
    for page in skarve.cleave(job, 1)? {
        let page = page?;
        rows += page["rows"].as_array().context("missing rows")?.len();
        complete = page["complete"] == true;
    }
    ensure!(complete && rows == 2, "batch was not fully consumed");
    println!(
        "{}",
        json!({"single":single, "compilation":compilation,
        "batch_rows":rows, "original_removed_before_skv_serving":true})
    );
    Ok(())
}
