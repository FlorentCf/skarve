use raster_engine::{
    coverage::compile_with_budget,
    model::{Grid, MAX_BYTES},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};

fn grid(width: usize, height: usize) -> Grid {
    Grid {
        width,
        height,
        transform: [0., 1., 0., height as f64, 0., -1.],
        crs: "LOCAL".to_owned(),
    }
}

fn strip(width: f64, epsilon: f64, rise: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[
        [0.25,1.], [width - 0.25,1. + rise],
        [width - 0.25,1. + rise + epsilon], [0.25,1. + epsilon], [0.25,1.]
    ]]})
}

#[test]
fn elongated_thin_polygon_uses_precision_path_above_absolute_area_threshold() {
    let epsilon = 1.5e-7;
    let geometry = strip(1024., epsilon, 1.);
    let plan = compile_with_budget(
        &grid(1024, 4),
        &geometry,
        "LOCAL",
        "scanline",
        &AtomicBool::new(false),
        MAX_BYTES,
    )
    .unwrap();
    // Use exact represented endpoint differences, not the nominal epsilon.
    let bottom_thickness = (1. + epsilon) - 1.;
    let top_thickness = (2. + epsilon) - 2.;
    let expected = 1023.5 * (bottom_thickness + top_thickness) / 2.;
    assert!(expected > 1e-4);
    assert_eq!(plan.strategy, "exact_rational_small_support");
    assert!((plan.selected - expected).abs() <= 1e-17);
    assert!((plan.polygon_area - expected).abs() <= 1e-17);
    assert_eq!(plan.intersecting, 1025);
    assert!(plan.spans.is_empty());
    assert!(plan.cells.iter().all(|cell| cell.fraction > 0.));
}

#[test]
fn precision_bbox_work_guard_rejects_before_clipping() {
    let error = compile_with_budget(
        &grid(40_000, 4),
        &strip(40_000., 1e-9, 0.),
        "LOCAL",
        "scanline",
        &AtomicBool::new(false),
        MAX_BYTES,
    )
    .unwrap_err();
    assert!(error.to_string().contains("precision fallback work budget"));
}

#[test]
fn precision_vertex_times_cell_work_guard_is_separate_from_bbox_limit() {
    let mut ring = Vec::new();
    for i in 0..128 {
        ring.push(json!([0.25 + 1999.5 * i as f64 / 127., 1.]));
    }
    for i in (0..128).rev() {
        ring.push(json!([0.25 + 1999.5 * i as f64 / 127., 1. + 1e-9]));
    }
    ring.push(ring[0].clone());
    let geometry = json!({"type":"Polygon","coordinates":[ring]});
    // Expanded bbox has 6000 cells, below100k, but6000×257 exceeds1m.
    let error = compile_with_budget(
        &grid(2000, 4),
        &geometry,
        "LOCAL",
        "scanline",
        &AtomicBool::new(false),
        MAX_BYTES,
    )
    .unwrap_err();
    assert!(error.to_string().contains("precision fallback work budget"));
}

#[test]
fn precision_memory_guard_rejects_before_rational_allocation() {
    let error = compile_with_budget(
        &grid(16, 16),
        &strip(16., 1e-12, 1.),
        "LOCAL",
        "scanline",
        &AtomicBool::new(false),
        128,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("precision fallback memory budget")
    );
}

#[test]
fn precision_cancellation_is_checked_before_work() {
    let error = compile_with_budget(
        &grid(16, 16),
        &strip(16., 1e-12, 1.),
        "LOCAL",
        "scanline",
        &AtomicBool::new(true),
        MAX_BYTES,
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[test]
fn active_precision_work_observes_cancellation() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Barrier::new(2));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_started = Arc::clone(&started);
    let worker = std::thread::spawn(move || {
        let geometry = strip(8192., 1e-9, 1.);
        worker_started.wait();
        compile_with_budget(
            &grid(8192, 4),
            &geometry,
            "LOCAL",
            "scanline",
            &worker_cancelled,
            MAX_BYTES,
        )
    });
    started.wait();
    // The24k+ rational candidate cells provide bounded sustained work. Avoid
    // asserting wall-clock latency, which is dependent on CI scheduling.
    std::thread::sleep(std::time::Duration::from_millis(5));
    cancelled.store(true, Ordering::Relaxed);
    let error = worker.join().unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}
