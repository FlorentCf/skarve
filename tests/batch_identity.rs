//! Future lazy declarations bind resume before any opening of that source.
use raster_engine::{
    batch::{Job, JobSpec, ResidentSource},
    model::RasterInput,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn definition() -> Value {
    json!({"zones":[{"id":"z","version":"1","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}],
        "slices":[{"id":"first","source":"r"},{"id":"future","spec":{"location":"/missing/future.nc","format":"netcdf","variable":"air","crs":"LOCAL","longitude_shift":0,"bands":[0],
        "identity":{"sha256":"ab".repeat(32),"byte_length":123,"policy":"verify","etag":"first"},"http":{"headers":{"X-Fixture":"old"}}}}],
        "crs":"LOCAL","tile_edge":32,"options":{"statistics":["sum","support"]}})
}
fn spec(value: &Value) -> JobSpec {
    serde_json::from_value(value.clone()).unwrap()
}

#[test]
fn unvisited_declarations_reject_changes_but_transport_can_move() {
    let input: RasterInput=serde_json::from_value(json!({"grid":{"width":1,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":[{"values":[3]}]})).unwrap();
    let raster = input.decode().unwrap();
    let definition = definition();
    let mut job = Job::new(spec(&definition), None, 16384, 1024 << 20).unwrap();
    let calls = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let page = job
        .next(
            1,
            |slice| {
                assert_eq!(slice.id, "first", "future lazy source must stay unopened");
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(ResidentSource::new(&raster)))
            },
            &cancel,
        )
        .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 1);
    let checkpoint = job.checkpoint();
    assert_eq!(checkpoint.next_row, 1);
    assert!(checkpoint.pins[0].is_some() && checkpoint.pins[1].is_none());
    let original = &definition["slices"][1]["spec"];
    let mut different_sha = original["identity"].clone();
    different_sha["sha256"] = json!("cd".repeat(32));
    let mut different_length = original["identity"].clone();
    different_length["byte_length"] = json!(124);
    for (field, value) in [
        ("format", json!("geotiff")),
        ("variable", json!("other")),
        ("crs", json!("OTHER")),
        ("longitude_shift", json!(-360)),
        ("bands", json!([1])),
        ("identity", different_sha),
        ("identity", different_length),
        ("identity", Value::Null),
    ] {
        let mut changed = definition.clone();
        changed["slices"][1]["spec"][field] = value;
        let error = Job::new(spec(&changed), Some(checkpoint.clone()), 16384, 1024 << 20)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("checkpoint incompatible"),
            "{field}: {error}"
        );
    }
    let mut moved = definition.clone();
    moved["slices"][1]["spec"]["location"] = json!("/different/missing/path.nc");
    moved["slices"][1]["spec"]["http"] = json!({"headers":{"X-Fixture":"new"},"header_env":{"X-Test":"MISSING_TEST_VAR"},"max_requests":2});
    moved["slices"][1]["spec"]["identity"]["policy"] = json!("trusted_manifest");
    moved["slices"][1]["spec"]["identity"]["etag"] = json!("second");
    moved["slices"][1]["spec"]["identity"]["sha256"] = json!("AB".repeat(32));
    let resumed = Job::new(spec(&moved), Some(checkpoint.clone()), 16384, 1024 << 20).unwrap();
    assert_eq!(resumed.checkpoint().fingerprint, checkpoint.fingerprint);
    assert_eq!(resumed.info()["next_row"], 1);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "construction/info must perform no source opens"
    );
}

#[test]
fn registered_slices_keep_the_prior_identity_shape() {
    let mut value = definition();
    value["slices"] = json!([{"id":"first","source":"r"},{"id":"future","source":"another"}]);
    let definition = spec(&value);
    let old_identity = json!({"zones":definition.zones.iter().map(|z|json!({"id":z.id,"version":z.version,"geometry":z.geometry})).collect::<Vec<_>>(),
        "slices":definition.slices.iter().map(|s|json!({"id":s.id,"time":s.time,"variable":s.variable,"bands":s.bands})).collect::<Vec<_>>(),
        "crs":definition.crs,"options":definition.options,"expression":definition.expression,"mask":definition.mask});
    let expected = blake3::hash(&serde_json::to_vec(&old_identity).unwrap())
        .to_hex()
        .to_string();
    let job = Job::new(definition, None, 16384, 1024 << 20).unwrap();
    assert_eq!(job.checkpoint().fingerprint, expected);
}
