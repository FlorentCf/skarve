use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{CarveOptions, Skarve, skv::CompileOptions};
use serde_json::{Value, json};
fn fixture(path: &std::path::Path) {
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(path, 4, 2, 1)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 2., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    let mut band = ds.rasterband(1).unwrap();
    band.set_no_data_value(Some(-9999.)).unwrap();
    band.set_scale(2.).unwrap();
    band.set_offset(1.).unwrap();
    band.write(
        (0, 0),
        (4, 2),
        &mut Buffer::new((4, 2), vec![1., 2., -9999., 4., 5., 6., 7., 8.]),
    )
    .unwrap();
    ds.flush_cache().unwrap();
}
fn zone() -> Value {
    json!({"type":"Polygon","coordinates":[[[0.,0.],[4.,0.],[4.,2.],[0.,2.],[0.,0.]]]})
}
fn job(path: &std::path::Path) -> Value {
    json!({"zones":[{"id":"a","version":"1","geometry":zone()}, {"id":"b","version":"1","geometry":zone()}],
        "slices":[{"id":"t","spec":{"location":path.to_str()}}], "crs":"EPSG:3857",
        "options":{"statistics":["sum","support","mean","min","max"]},
        "budget":{"working_bytes":67108864,"geometry_bytes":8388608,"tile_bytes":16777216,"output_bytes":1048576}})
}
#[test]
fn owned_direct_compile_standalone_and_streamed_batch() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.tif");
    let skv = dir.path().join("standalone.skv");
    fixture(&original);
    let mut engine = Skarve::new();
    let options = CarveOptions {
        metrics: ["sum", "support", "mean", "min", "max"]
            .map(str::to_owned)
            .to_vec(),
        ..Default::default()
    };
    let direct = {
        let mut source = engine.infuse(original.to_str().unwrap()).unwrap();
        let result = source.carve(&zone(), &options).unwrap();
        assert_eq!(result["bands"][0]["fractional_sum"], 73.0);
        assert_eq!(result["bands"][0]["covered_cell_equivalents"], 7.0);
        assert!(
            source
                .carve_with_options(&zone(), json!({"unknown_option":1}))
                .is_err()
        );
        assert!(
            source
                .carve_with_options(&zone(), json!({"source":"other"}))
                .is_err()
        );
        assert!(source.carve_with_options(&zone(), json!({"backend":"exactextract", "numerical_policy":"native_grid_planar_fractional"})).is_err());
        source
            .compile(
                skv.to_str().unwrap(),
                &CompileOptions {
                    chunk_edge: 64,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            source
                .compile(skv.to_str().unwrap(), &CompileOptions::default())
                .is_err()
        );
        result
    };
    std::fs::remove_file(&original).unwrap();
    {
        let mut source = engine.infuse(skv.to_str().unwrap()).unwrap();
        assert_eq!(
            source.carve(&zone(), &options).unwrap()["bands"],
            direct["bands"]
        );
        assert!(source.inspect().is_ok());
        source.close().unwrap();
    }
    let pages: Vec<_> = engine
        .cleave(job(&skv), 1)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        pages
            .iter()
            .map(|p| p["rows"].as_array().unwrap().len())
            .sum::<usize>(),
        2
    );
    assert_eq!(pages.last().unwrap()["complete"], true);
    assert_eq!(
        engine.request(json!({"op":"stats"})).unwrap()["resident_bytes"],
        0
    );
}
#[test]
fn cancellation_is_sticky_and_drops_release_registrations() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("test.tif");
    fixture(&original);
    let mut engine = Skarve::new();
    let signal = engine.cancellation();
    {
        let mut source = engine.infuse(original.to_str().unwrap()).unwrap();
        signal.cancel();
        assert!(source.carve(&zone(), &CarveOptions::default()).is_err());
        source.close().unwrap(); // cleanup works after cancellation
    }
    assert!(engine.infuse(original.to_str().unwrap()).is_err());
    engine.reset_cancellation();
    {
        let mut batch = engine.cleave(job(&original), 1).unwrap();
        signal.cancel();
        assert!(batch.next().unwrap().is_err());
        assert!(batch.next().is_none());
    }
    engine.reset_cancellation();
    assert_eq!(
        engine.request(json!({"op":"stats"})).unwrap()["resident_bytes"],
        0
    );
    assert!(engine.infuse(original.to_str().unwrap()).is_ok());
}
#[test]
fn early_drop_and_page_budget_validation() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("test.tif");
    fixture(&original);
    let mut engine = Skarve::new();
    assert!(engine.cleave(job(&original), 0).is_err());
    assert!(engine.cleave(job(&original), 4097).is_err());
    {
        let mut batch = engine.cleave(job(&original), 1).unwrap();
        assert!(batch.next().unwrap().is_ok());
    }
    assert_eq!(
        engine.request(json!({"op":"stats"})).unwrap()["resident_bytes"],
        0
    );
}
