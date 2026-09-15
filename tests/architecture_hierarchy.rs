use raster_engine::{
    aggregate::{self, Options},
    coverage,
    hierarchy::Hierarchy,
    model::{Band, Grid, Raster, Sum},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

fn raster(w: usize, h: usize, n: usize) -> Raster {
    Raster {
        grid: Grid {
            width: w,
            height: h,
            transform: [0., 1., 0., 0., 0., -1.],
            crs: "LOCAL".into(),
        },
        source_id: "hierarchy-test-immutable".into(),
        bands: (0..n)
            .map(|bi| Band {
                values: (0..w * h)
                    .map(|i| ((i * 17 + bi * 31) % 113) as f64 * 0.25 - 11.5)
                    .collect(),
                valid: (0..w * h).map(|i| (i + bi * 3) % 19 != 0).collect(),
                unit: Some("scaled native value".into()),
            })
            .collect(),
    }
}
fn polygon(rings: Vec<Vec<[f64; 2]>>) -> Value {
    json!({"type":"Polygon","coordinates":rings.into_iter().map(|r|r.into_iter().map(|[x,y]|[x,-y]).collect::<Vec<_>>()).collect::<Vec<_>>()})
}
fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
    vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]
}
fn assert_close(a: &Value, b: &Value, path: &str) {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            assert!(
                (x - y).abs() <= 2e-9 + 2e-12 * y.abs(),
                "{path}: {x} != {y}"
            );
        }
        (Value::Object(x), Value::Object(y)) => {
            assert_eq!(x.len(), y.len(), "{path}");
            for (k, v) in x {
                assert_close(v, &y[k], &format!("{path}.{k}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            assert_eq!(x.len(), y.len(), "{path}");
            for (i, (a, b)) in x.iter().zip(y).enumerate() {
                assert_close(a, b, &format!("{path}[{i}]"));
            }
        }
        _ => assert_eq!(a, b, "{path}"),
    }
}

#[test]
fn architecture_hierarchy_nonpower2_multiband_statistics_match_direct() {
    let r = raster(37, 29, 4);
    let cancel = AtomicBool::new(false);
    let geometries = vec![
        polygon(vec![rect(-2., -3., 39., 31.)]),
        polygon(vec![rect(0.3, 0.7, 34.8, 27.2), rect(3.1, 4.2, 8.7, 9.4)]),
        polygon(vec![vec![
            [0.1, 1.3],
            [34.7, 26.4],
            [32.9, 28.2],
            [1.3, 3.1],
            [0.1, 1.3],
        ]]),
        json!({"type":"MultiPolygon","coordinates":[polygon(vec![rect(1.2,2.3,7.4,8.9)])["coordinates"],polygon(vec![rect(23.5,19.1,31.3,27.7)])["coordinates"]]}),
        polygon(vec![]),
        polygon(vec![rect(42., 35., 44., 37.)]),
    ];
    let options = vec![
        Options::default(),
        Options {
            statistics: Some(vec!["sum".into()]),
            ..Default::default()
        },
        Options {
            statistics: Some(vec!["mean".into()]),
            ..Default::default()
        },
        Options {
            statistics: Some(vec!["support".into(), "min".into(), "max".into()]),
            ..Default::default()
        },
        Options {
            histogram_edges: Some(vec![-8., 0., 5., 12.]),
            statistics: Some(vec!["histogram".into()]),
            ..Default::default()
        },
        Options {
            bands: vec![0, 2],
            weight_band: Some(3),
            statistics: Some(vec!["weighted_mean".into()]),
            ..Default::default()
        },
    ];
    for leaf in [1, 16, 64, 256] {
        let h = Hierarchy::build(&r, leaf, &cancel).unwrap();
        for g in &geometries {
            let plan = coverage::compile_polygon(&r.grid, g, "LOCAL", "direct", &cancel).unwrap();
            for o in &options {
                let actual = h.query(&r, g, "LOCAL", o, &cancel).unwrap();
                let reference = aggregate::measure(&r, &plan, o, None, &cancel).unwrap();
                assert_close(
                    &serde_json::to_value(actual.bands).unwrap(),
                    &serde_json::to_value(reference).unwrap(),
                    "bands",
                );
                assert!(!actual.diagnostics.full_resolution_plan_created);
            }
        }
    }
}

#[test]
fn architecture_hierarchy_wholly_contained_hole_has_independent_area_and_extrema() {
    let mut r = raster(64, 64, 1);
    r.bands[0].values.fill(2.);
    r.bands[0].valid.fill(true);
    // The hole is strictly within a quadrant; no quadrant corner is in it.
    r.bands[0].values[7 * 64 + 6] = -999.;
    let g = polygon(vec![rect(-1., -1., 65., 65.), rect(5., 6., 8., 9.)]);
    let result = Hierarchy::build(&r, 4, &AtomicBool::new(false))
        .unwrap()
        .query(
            &r,
            &g,
            "LOCAL",
            &Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
    let b = &result.bands[0];
    assert_eq!(b.selected_cell_equivalents, 4096. - 9.);
    assert_eq!(b.covered_cell_equivalents, 4087.);
    assert_eq!(b.fractional_sum, 8174.);
    assert_eq!(b.valid_cell_count, 4087);
    assert_eq!(b.min, Some(2.));
    assert_eq!(b.max, Some(2.));
    assert!(result.diagnostics.avoided_raw_band_cell_visits > 3000);
    assert!(result.diagnostics.boundary_leaves > 0);
}

#[test]
fn architecture_hierarchy_full_node_avoids_raw_and_preserves_compensation() {
    let mut r = raster(129, 97, 1);
    r.bands[0].valid.fill(true);
    r.bands[0].values.fill(0.);
    // Child totals contain corrections that must survive the parent merge.
    for row in 0..97 {
        let i = row * 129;
        r.bands[0].values[i] = 1e16;
        r.bands[0].values[i + 1] = 1.;
        r.bands[0].values[i + 2] = -1e16;
    }
    let g = polygon(vec![rect(-1., -1., 130., 98.)]);
    let h = Hierarchy::build(&r, 16, &AtomicBool::new(false)).unwrap();
    let result = h
        .query(
            &r,
            &g,
            "LOCAL",
            &Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(result.bands[0].fractional_sum, 97.);
    assert_eq!(result.diagnostics.nodes_visited, 1);
    assert_eq!(result.diagnostics.raw_band_cell_visits, 0);
    assert_eq!(result.diagnostics.avoided_raw_band_cell_visits, 129 * 97);
}

#[test]
fn architecture_hierarchy_fractional_hole_independent_rectangle_oracle() {
    let r = raster(41, 35, 2);
    let outer = [0.25, 0.75, 39.125, 33.625];
    let hole = [5.125, 6.25, 8.625, 9.75];
    let g = polygon(vec![
        rect(outer[0], outer[1], outer[2], outer[3]),
        rect(hole[0], hole[1], hole[2], hole[3]),
    ]);
    let result = Hierarchy::build(&r, 4, &AtomicBool::new(false))
        .unwrap()
        .query(
            &r,
            &g,
            "LOCAL",
            &Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
    for (bi, band) in r.bands.iter().enumerate() {
        let mut sum = Sum::default();
        let mut support = Sum::default();
        let mut selected = Sum::default();
        let mut count = 0;
        for y in 0..35 {
            for x in 0..41 {
                let area = |a: [f64; 4]| {
                    ((x as f64 + 1.).min(a[2]) - (x as f64).max(a[0])).max(0.)
                        * ((y as f64 + 1.).min(a[3]) - (y as f64).max(a[1])).max(0.)
                };
                let f = area(outer) - area(hole);
                selected.add(f);
                if f > 0. && band.valid[y * 41 + x] {
                    sum.add(f * band.values[y * 41 + x]);
                    support.add(f);
                    count += 1;
                }
            }
        }
        let b = &result.bands[bi];
        assert_eq!(b.fractional_sum, sum.value());
        assert_eq!(b.covered_cell_equivalents, support.value());
        assert_eq!(b.selected_cell_equivalents, selected.value());
        assert_eq!(b.valid_cell_count, count);
    }
}

#[test]
fn architecture_hierarchy_tiny_affine_and_elongated_sliver_reuse_exact_contract() {
    let mut r = raster(16, 4, 2);
    r.grid.transform = [0.1, 0.2, 0., 0.3, 0., -0.2];
    let g = json!({"type":"Polygon","coordinates":[[[0.5,0.25],[0.5+1e-12,0.25],[0.5+1e-12,0.2],[0.5,0.2],[0.5,0.25]]]});
    let cancel = AtomicBool::new(false);
    let h = Hierarchy::build(&r, 1, &cancel).unwrap();
    let actual = h
        .query(&r, &g, "LOCAL", &Options::default(), &cancel)
        .unwrap();
    let p = coverage::compile_polygon(&r.grid, &g, "LOCAL", "scanline", &cancel).unwrap();
    assert!(actual.diagnostics.precise_boundary_fallback);
    assert_eq!(p.strategy, "exact_rational_small_support");
    assert_eq!(actual.diagnostics.nodes_inside, 0);
    assert_eq!(
        serde_json::to_value(actual.bands).unwrap(),
        serde_json::to_value(
            aggregate::measure(&r, &p, &Options::default(), None, &cancel).unwrap()
        )
        .unwrap()
    );
}

#[test]
fn architecture_hierarchy_auxiliary_fallback_is_accounted_and_negative_weights_excluded() {
    let r = raster(65, 49, 2);
    let cancel = AtomicBool::new(false);
    let h = Hierarchy::build(&r, 16, &cancel).unwrap();
    let o = Options {
        weight_band: Some(1),
        histogram_edges: Some(vec![-12., 0., 20.]),
        ..Default::default()
    };
    let g = polygon(vec![rect(-1., -1., 66., 50.)]);
    let result = h.query(&r, &g, "LOCAL", &o, &cancel).unwrap();
    assert_eq!(result.diagnostics.avoided_raw_band_cell_visits, 0);
    assert_eq!(result.diagnostics.raw_band_cell_visits, 65 * 49 * 2);
    assert!(result.diagnostics.auxiliary_raw_fallback);
    let p = coverage::compile_polygon(&r.grid, &g, "LOCAL", "direct", &cancel).unwrap();
    assert_close(
        &serde_json::to_value(result.bands).unwrap(),
        &serde_json::to_value(aggregate::measure(&r, &p, &o, None, &cancel).unwrap()).unwrap(),
        "bands",
    );
}

#[test]
fn architecture_hierarchy_rejects_invalid_geometry_options_identity_and_cancellation() {
    let mut r = raster(17, 13, 1);
    let c = AtomicBool::new(false);
    let h = Hierarchy::build(&r, 4, &c).unwrap();
    let g = polygon(vec![rect(-1., -1., 18., 14.)]);
    assert!(Hierarchy::build(&r, 0, &c).is_err());
    assert!(Hierarchy::build(&r, 4, &AtomicBool::new(true)).is_err());
    assert!(h.query(&r, &g, "OTHER", &Options::default(), &c).is_err());
    assert!(
        h.query(
            &r,
            &g,
            "LOCAL",
            &Options {
                statistics: Some(vec!["histogram".into()]),
                ..Default::default()
            },
            &c
        )
        .is_err()
    );
    let invalid = polygon(vec![vec![[0., 0.], [4., 4.], [0., 4.], [4., 0.], [0., 0.]]]);
    assert!(
        h.query(&r, &invalid, "LOCAL", &Options::default(), &c)
            .is_err()
    );
    assert!(
        h.query(&r, &g, "LOCAL", &Options::default(), &AtomicBool::new(true))
            .is_err()
    );
    r.source_id = "different".into();
    assert!(h.query(&r, &g, "LOCAL", &Options::default(), &c).is_err());
}
