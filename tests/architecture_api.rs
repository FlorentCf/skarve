use raster_engine::session::Session;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

fn source(values: Vec<Vec<f64>>) -> Session {
    let mut s = Session::default();
    let width = values[0].len();
    let bands: Vec<Value> = values
        .into_iter()
        .map(|values| json!({"values":values}))
        .collect();
    s.call(json!({"op":"open","id":"r","raster":{"grid":{"width":width,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":bands}}),&AtomicBool::new(false)).unwrap();
    s
}
fn query(width: usize) -> Value {
    json!({"op":"measure","source":"r","geometry":{"type":"Polygon","coordinates":[[[0,0],[width,0],[width,1],[0,1],[0,0]]]},"crs":"LOCAL"})
}

#[test]
fn unrequested_overflowing_arithmetic_is_not_evaluated() {
    let cancel = AtomicBool::new(false);
    let mut s = source(vec![vec![1e308, 1e308]]);
    let mut q = query(2);
    q["statistics"] = json!(["support"]);
    let a = s.call(q.clone(), &cancel).unwrap();
    assert_eq!(a["bands"][0]["covered_cell_equivalents"], 2.);
    assert!(a["bands"][0].get("fractional_sum").is_none());
    q["statistics"] = json!(["sum"]);
    assert!(s.call(q, &cancel).is_err());
    for backend in ["direct", "hierarchy"] {
        let mut s = source(vec![vec![1e308], vec![1e308]]);
        if backend == "hierarchy" {
            s.call(
                json!({"op":"prepare","source":"r","backend":backend,"tile_edge":1}),
                &cancel,
            )
            .unwrap();
        }
        let mut q = query(1);
        q["backend"] = json!(backend);
        q["bands"] = json!([0]);
        q["statistics"] = json!(["weight_sum"]);
        q["weight_band"] = json!(1);
        let a = s.call(q.clone(), &cancel).unwrap();
        assert_eq!(a["bands"][0]["weight_sum"], json!(1e308));
        assert!(a["bands"][0].get("weighted_sum").is_none());
        q["statistics"] = json!(["weighted_sum"]);
        assert!(s.call(q, &cancel).is_err());
    }
    let mut s = source(vec![vec![0., 0.], vec![1e308, 1e308]]);
    let mut q = query(2);
    q["bands"] = json!([0]);
    q["statistics"] = json!(["weighted_sum"]);
    q["weight_band"] = json!(1);
    assert_eq!(s.call(q, &cancel).unwrap()["bands"][0]["weighted_sum"], 0.);
}

#[test]
fn standard_session_routes_candidates_and_rejects_conflicting_options() {
    let cancel = AtomicBool::new(false);
    let mut s = source(vec![vec![1., -2., 3., 4.]]);
    for backend in [
        "row_blocks",
        "hierarchy",
        "cumulative_full",
        "cumulative_blocked",
    ] {
        s.call(
            json!({"op":"prepare","source":"r","backend":backend}),
            &cancel,
        )
        .unwrap();
        let mut q = query(4);
        q["backend"] = json!(backend);
        q["statistics"] = json!(["sum", "mean"]);
        let a = s.call(q.clone(), &cancel).unwrap();
        assert_eq!(a["bands"][0]["fractional_sum"], 6.);
        assert_eq!(a["bands"][0]["coverage_weighted_mean"], 1.5);
        if backend != "row_blocks" {
            q["use_index"] = json!(false);
            assert!(s.call(q, &cancel).is_err());
        }
    }
    let mut q = query(4);
    q["backend"] = json!("cumulative_full");
    q["statistics"] = json!(["min", "max"]);
    let a = s.call(q, &cancel).unwrap();
    assert_eq!(a["bands"][0]["min"], -2.);
    assert!(
        a["selection_reason"]
            .as_str()
            .unwrap()
            .contains("requires scanline")
    );
    assert!(
        s.call(
            json!({"op":"prepare","source":"r","backend":"cumulative_blocked","columns":2}),
            &cancel
        )
        .is_err()
    );
    s.call(json!({"op":"close","source":"r"}), &cancel).unwrap();
    assert_eq!(s.bytes(), 0);
}

#[test]
fn bounded_malformed_requests_return_errors_without_panicking() {
    let cancel = AtomicBool::new(false);
    let mutations = [
        ("statistics", json!([])),
        ("statistics", json!(["sum", "sum"])),
        ("statistics", json!(["unsupported_statistic"])),
        ("statistics", json!(["histogram"])),
        ("statistics", json!(["weighted_mean"])),
        ("backend", json!(5)),
        ("backend", json!("invented")),
        ("strategy", json!("invented")),
        ("bands", json!([usize::MAX])),
        ("bands", json!([0, 0])),
    ];
    for (field, value) in mutations {
        let mut s = source(vec![vec![1.]]);
        let mut q = query(1);
        q[field] = value;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.call(q, &cancel)));
        assert!(result.is_ok(), "panic on {field}");
        assert!(result.unwrap().is_err(), "accepted {field}");
    }
}
