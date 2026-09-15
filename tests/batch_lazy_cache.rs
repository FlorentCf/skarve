//! Actual-file one-entry source reuse, invalidation, and pre-open reservation.
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::batch::{Job, JobSpec};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn fixture(path: &Path, offset: f64) {
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let mut dataset = driver
        .create_with_band_type::<f64, _>(path, 2, 1, 2)
        .unwrap();
    dataset
        .set_geo_transform(&[0., 1., 0., 1., 0., -1.])
        .unwrap();
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for (i, values) in [
        (1, vec![2. + offset, 4. + offset]),
        (2, vec![20. + offset, 40. + offset]),
    ] {
        dataset
            .rasterband(i)
            .unwrap()
            .write((0, 0), (2, 1), &mut Buffer::new((2, 1), values))
            .unwrap();
    }
    dataset.flush_cache().unwrap();
}
fn definition(path: &Path) -> Value {
    let source = json!({"location":path,"bands":[0,1]});
    let mut slices: Vec<Value> = (0..4)
        .map(|i| json!({"id":i.to_string(),"spec":source,"bands":[i%2]}))
        .collect();
    slices.push(json!({"id":"other","spec":{"location":path,"bands":[1,0]},"bands":[0]}));
    slices.push(json!({"id":"revisit","spec":source,"bands":[0]}));
    json!({"zones":[{"id":"z","version":"1","geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,1],[0,1],[0,0]]]}}],
        "slices":slices,"crs":"EPSG:3857","tile_edge":32,"options":{"statistics":["sum","support"]}})
}
fn spec(value: &Value) -> JobSpec {
    serde_json::from_value(value.clone()).unwrap()
}

#[test]
fn one_reader_serves_four_slices_and_reopens_only_when_spec_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, 0.);
    let definition = definition(&path);
    let cancel = AtomicBool::new(false);
    let mut job = Job::new(spec(&definition), None, 16384, 1024 << 20).unwrap();
    let mut rows = Vec::new();
    let mut checkpoint = None;
    for i in 0..6 {
        let page = job
            .next_with_checkpoint(
                1,
                false,
                |_| panic!("lazy specs use the owned original source"),
                &cancel,
            )
            .unwrap();
        rows.extend(page["rows"].as_array().unwrap().iter().cloned());
        if i == 1 {
            checkpoint = Some(job.checkpoint());
        }
        if i == 3 {
            assert_eq!(job.metrics.source_opens, 1);
            assert_eq!(job.metrics.lazy_source_cache_hits, 3);
        }
        assert_eq!(page.get("checkpoint").is_some(), i == 5);
    }
    assert_eq!(
        rows.iter()
            .map(|r| r["bands"][0]["fractional_sum"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        vec![6., 60., 6., 60., 60., 6.]
    );
    assert_eq!(job.metrics.source_acquisitions, 6);
    assert_eq!(job.metrics.source_opens, 3);
    assert_eq!(job.metrics.lazy_source_cache_hits, 3);
    let mut resumed = Job::new(spec(&definition), checkpoint, 16384, 1024 << 20).unwrap();
    let mut tail = Vec::new();
    loop {
        let page = resumed
            .next(1, |_| panic!("lazy resume must use owned source"), &cancel)
            .unwrap();
        tail.extend(page["rows"].as_array().unwrap().iter().cloned());
        if page["complete"] == true {
            break;
        }
    }
    assert_eq!(tail, rows[2..]);
    assert_eq!(resumed.metrics.source_opens, 3);
}

#[test]
fn cached_source_change_and_retry_fail_before_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, 0.);
    let mut job = Job::new(spec(&definition(&path)), None, 16384, 1024 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    job.next(1, |_| panic!(), &cancel).unwrap();
    let replacement = dir.path().join("replacement.tif");
    fixture(&replacement, 100.);
    std::fs::rename(replacement, &path).unwrap();
    for _ in 0..2 {
        let error = job.next(1, |_| panic!(), &cancel).unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        assert_eq!(job.checkpoint().next_row, 1);
        assert_eq!(job.metrics.output_rows, 1);
        assert_eq!(job.metrics.windows_read, 1);
    }
}

#[test]
fn decoded_hit_does_not_bypass_original_source_mutation_checks() {
    use raster_engine::tile_cache::TileCache;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, 0.);
    let mut definition = definition(&path);
    definition["slices"][1]["bands"] = json!([0]);
    let mut job = Job::new(spec(&definition), None, 16384, 1024 << 20).unwrap();
    let mut cache = TileCache::default();
    cache.set_limit(1 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    job.next_with_cache(1, false, |_| panic!(), &cancel, &mut cache)
        .unwrap();
    assert!(!cache.is_empty());
    let replacement = dir.path().join("replacement.tif");
    fixture(&replacement, 100.);
    std::fs::rename(replacement, &path).unwrap();
    for _ in 0..2 {
        let error = job
            .next_with_cache(1, false, |_| panic!(), &cancel, &mut cache)
            .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        assert_eq!(job.metrics.decoded_cache_hits, 0);
        assert_eq!(job.metrics.windows_read, 1);
        assert_eq!(job.checkpoint().next_row, 1);
    }
}

#[test]
fn reader_memory_and_cancellation_reject_before_any_original_open() {
    let path = Path::new("/deliberately/missing/source.tif");
    let mut low = definition(path);
    low["budget"] = json!({"working_bytes":16<<20,"geometry_bytes":1<<20,"tile_bytes":1<<20,"output_bytes":1<<20});
    let mut job = Job::new(spec(&low), None, 16384, 1024 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let error = job.next(1, |_| panic!(), &cancel).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("lazy source reader exceeds working budget"),
        "{error}"
    );
    assert_eq!(job.metrics.source_opens, 0);
    assert_eq!(job.checkpoint().next_row, 0);
    cancel.store(true, Ordering::Relaxed);
    assert!(
        job.next(1, |_| panic!(), &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_eq!(job.metrics.source_opens, 0);
}

#[test]
fn new_geometry_uses_residual_working_budget_before_it_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, 0.);
    let mut definition = definition(&path);
    let zone = definition["zones"][0].clone();
    definition["zones"] = Value::Array(
        (0..1024)
            .map(|i| {
                let mut z = zone.clone();
                z["id"] = json!(i.to_string());
                z
            })
            .collect(),
    );
    definition["slices"] = json!([definition["slices"][0]]);
    definition["budget"] = json!({"working_bytes":22<<20,"geometry_bytes":12<<20,"tile_bytes":1<<20,"output_bytes":1<<20});
    let spec_bytes = serde_json::to_vec(&definition).unwrap().len() * 4;
    let mut job = Job::new(spec(&definition), None, spec_bytes, 1024 << 20).unwrap();
    let error = job
        .next(1, |_| panic!(), &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("budget"), "{error}");
    assert_eq!(
        job.metrics.source_opens, 1,
        "reader fits the initial preflight"
    );
    assert_eq!(
        job.metrics.geometry_compilations, 0,
        "new geometry must be bounded before insertion"
    );
    assert_eq!(job.metrics.geometry_bytes, 0);
    assert_eq!(job.metrics.windows_read, 0);
    assert_eq!(job.metrics.output_rows, 0);
}
