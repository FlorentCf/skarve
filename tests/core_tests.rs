use raster_engine::{model::Sum, session::Session};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
fn call(s: &mut Session, v: Value) -> Value {
    s.call(v, &AtomicBool::new(false)).unwrap()
}
fn open(s: &mut Session, values: Vec<f64>) {
    let n = values.len();
    call(
        s,
        json!({"op":"open","id":"r","raster":{"grid":{"width":n,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":[{"values":values}]}}),
    );
}
fn polygon(x0: f64, x1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,0.],[x1,0.],[x1,1.],[x0,1.],[x0,0.]]]})
}
#[test]
fn compensated_block_retains_cancellation_residual() {
    let mut s = Session::default();
    let values = (0..512)
        .map(|i| match i % 4 {
            0 => 1e16,
            1 => 1.,
            2 => -1e16,
            _ => -2.,
        })
        .collect();
    open(&mut s, values);
    let q = json!({"op":"measure","source":"r","geometry":polygon(0.,512.),"crs":"LOCAL"});
    let direct = call(&mut s, q.clone());
    assert_eq!(direct["bands"][0]["fractional_sum"], -128.);
    call(&mut s, json!({"op":"prepare","source":"r"}));
    let indexed = call(&mut s, q);
    assert_eq!(direct["bands"], indexed["bands"]);
}
#[test]
fn cancellation_is_error_before_work() {
    let mut s = Session::default();
    assert!(
        s.call(json!({"op":"stats"}), &AtomicBool::new(true))
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}
#[test]
fn overflow_fails_instead_of_null() {
    let mut s = Session::default();
    open(&mut s, vec![1e308, 1e308]);
    assert!(
        s.call(
            json!({"op":"measure","source":"r","geometry":polygon(0.,2.),"crs":"LOCAL"}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn sum_merge_preserves_compensation() {
    let mut a = Sum::default();
    a.add(1e16);
    a.add(1.);
    let mut b = Sum::default();
    b.add(-1e16);
    a.merge(b);
    assert_eq!(a.value(), 1.);
}
#[test]
fn source_cannot_be_mutated_behind_index() {
    let mut s = Session::default();
    open(&mut s, vec![1., 2.]);
    assert!(
        s.call(
            json!({"op":"open","id":"r","raster":{}}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn mode_is_never_silently_changed() {
    let mut s = Session::default();
    assert!(
        s.call(
            json!({"op":"stats","mode":"spherical"}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn unsupported_options_are_never_silently_ignored() {
    let mut s = Session::default();
    for v in [
        json!({"op":"stats","statistics":["variance"]}),
        json!({"op":"open","id":"r","path":"a","raster":{}}),
        json!({"op":"measure","source":"r","strategy":123}),
        json!({"op":"measure","source":"r","use_index":"false"}),
    ] {
        assert!(s.call(v, &AtomicBool::new(false)).is_err());
    }
}
#[test]
fn preflight_allocation_and_cancelled_decode() {
    use raster_engine::model::RasterInput;
    let v = json!({"grid":{"width":2,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":[{"values":[1,2]}]});
    let input: RasterInput = serde_json::from_value(v.clone()).unwrap();
    assert!(
        input
            .decode_with_budget(8, &AtomicBool::new(false))
            .is_err()
    );
    let input: RasterInput = serde_json::from_value(v).unwrap();
    assert!(
        input
            .decode_with_budget(1024, &AtomicBool::new(true))
            .is_err()
    );
}
#[test]
fn topology_error_collection_is_bounded() {
    let mut s = Session::default();
    open(&mut s, vec![1., 2.]);
    let parts = vec![polygon(0., 1.)["coordinates"].clone(); 129];
    let e=s.call(json!({"op":"measure","source":"r","crs":"LOCAL","geometry":{"type":"MultiPolygon","coordinates":parts}}),&AtomicBool::new(false)).unwrap_err();
    assert!(e.to_string().contains("component-count"));
}
