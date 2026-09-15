use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::session::{Session, request};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

fn call(session: &mut Session, value: Value) -> Value {
    serde_json::from_str(&request(
        session,
        &value.to_string(),
        &AtomicBool::new(false),
    ))
    .unwrap()
}

#[test]
fn ordinary_session_requires_explicit_ordered_policy_and_preserves_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&path, 3, 2, 1)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 2., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    let mut band = ds.rasterband(1).unwrap();
    band.set_no_data_value(Some(-99999.)).unwrap();
    band.set_scale(2.).unwrap();
    band.set_offset(3.).unwrap();
    band.write(
        (0, 0),
        (3, 2),
        &mut Buffer::new((3, 2), vec![1f32, -2., -99999., 0., 4., 5.]),
    )
    .unwrap();
    ds.flush_cache().unwrap();
    drop(ds);
    let mut session = Session::default();
    assert_eq!(
        call(
            &mut session,
            json!({"op":"register_source","id":"s","spec":{"location":path}})
        )["ok"],
        true
    );
    let selections = json!({"polygons":[{"id":"a","windows":[{"window":[0,0,3,2],"runs":[[0,6]]}]}],"nodata":[-99999.]});
    let mut command = json!({"op":"measure_ordered_source","source":"s","request":selections});
    assert_eq!(call(&mut session, command.clone())["ok"], false);
    for policy in [
        "native_grid_planar_fractional",
        "exactextract_fractional_v030",
        "strict_selected_v1",
    ] {
        command["numerical_policy"] = policy.into();
        assert_eq!(call(&mut session, command.clone())["ok"], false);
    }
    command["numerical_policy"] = "hm_demographics_ordered_v1".into();
    let result = call(&mut session, command.clone());
    assert_eq!(result["ok"], true, "{result}");
    let band = &result["result"]["rows"][0]["bands"][0];
    assert_eq!(band["sum"], 10.);
    assert_eq!(band["valid_count"], 4);
    assert_eq!(band["excluded_negative"], 1);
    assert_eq!(band["excluded_nodata"], 1);
    for (key, value) in [
        ("backend", json!("exactextract")),
        ("mode", json!("native_grid_planar")),
        ("statistics", json!(["sum"])),
    ] {
        let mut rejected = command.clone();
        rejected[key] = value;
        assert_eq!(call(&mut session, rejected)["ok"], false);
    }
    let geometry = json!({"type":"Polygon","coordinates":[[[0,0],[3,0],[3,2],[0,2],[0,0]]]});
    let ordinary = json!({"op":"measure_source","source":"s","geometry":geometry,"crs":"EPSG:3857","statistics":["sum"]});
    let native = call(&mut session, ordinary.clone());
    assert_eq!(native["ok"], true, "{native}");
    assert_eq!(native["result"]["bands"][0]["fractional_sum"], 31.);
    let mut incompatible = ordinary;
    incompatible["numerical_policy"] = "hm_demographics_ordered_v1".into();
    assert_eq!(call(&mut session, incompatible)["ok"], false);
    assert_eq!(
        call(&mut session, json!({"op":"close_source","source":"s"}))["ok"],
        true
    );
    assert_eq!(call(&mut session, command)["ok"], false);
}
