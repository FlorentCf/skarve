use raster_engine::{
    aggregate, coverage,
    cumulative::{CumulativeField, Origin},
    model::{Band, Grid, Raster, Sum},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

fn raster(width: usize, height: usize) -> Raster {
    let grid = Grid {
        width,
        height,
        transform: [0., 1., 0., 0., 0., -1.],
        crs: "EPSG:3857".into(),
    };
    Raster {
        grid,
        bands: (0..3)
            .map(|b| Band {
                values: (0..width * height)
                    .map(|i| ((i * 17 + b * 13) % 103) as f64 * 0.25 - 12.)
                    .collect(),
                valid: (0..width * height).map(|i| (i + b * 7) % 11 != 0).collect(),
                unit: None,
            })
            .collect(),
        source_id: "cumulative-test-immutable".into(),
    }
}
fn polygon(rings: Vec<Vec<[f64; 2]>>) -> Value {
    json!({"type":"Polygon","coordinates":rings.into_iter().map(|r|r.into_iter().map(|p|[p[0],-p[1]]).collect::<Vec<_>>()).collect::<Vec<_>>()})
}
fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    polygon(vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]])
}
fn close(a: f64, b: f64) {
    assert!(
        (a - b).abs() <= 1e-8 + 1e-10 * b.abs(),
        "{a} != {b}, error {}",
        a - b
    );
}
fn reference(r: &Raster, p: &Value) -> Vec<aggregate::BandResult> {
    let c = AtomicBool::new(false);
    let plan = coverage::compile_polygon(&r.grid, p, &r.grid.crs, "direct", &c).unwrap();
    aggregate::measure(r, &plan, &aggregate::Options::default(), None, &c).unwrap()
}

#[test]
fn architecture_cumulative_equal_world_coordinates_preserve_horizontal_identity() {
    let mut r = raster(32, 16);
    r.grid.transform = [0.1, 0.2, 0., 4.1, 0., -0.2];
    let input = json!({"type":"Polygon","coordinates":[[[0.35,1.35],[5.55,1.35],[5.55,3.75],[0.35,3.75],[0.35,1.35]]]});
    let expected = reference(&r, &input);
    let cancel = AtomicBool::new(false);
    for origin in [Origin::Full, Origin::Blocked { columns: 8 }] {
        let index = CumulativeField::build(&r, origin, &cancel).unwrap();
        let answer = index.query(&r, &input, &cancel).unwrap();
        assert!(
            answer.diagnostics.strategy.starts_with("cumulative_"),
            "{:?}",
            answer.diagnostics
        );
        for (actual, expected) in answer.bands.iter().zip(&expected) {
            close(actual.fractional_sum, expected.fractional_sum);
            close(
                actual.covered_cell_equivalents,
                expected.covered_cell_equivalents,
            );
        }
    }
}

#[test]
fn architecture_cumulative_rectangles_independent_cell_oracle() {
    let r = raster(80, 64);
    let c = AtomicBool::new(false);
    for origin in [
        Origin::Full,
        Origin::Blocked { columns: 8 },
        Origin::Blocked { columns: 31 },
    ] {
        let ix = CumulativeField::build(&r, origin, &c).unwrap();
        for [x0, y0, x1, y1] in [
            [1.25, 2.5, 75.75, 60.125],
            [-5., -7., 22.25, 29.75],
            [33., 12., 49., 48.],
            [81., 0., 90., 60.],
        ] {
            let got = ix.query(&r, &rectangle(x0, y0, x1, y1), &c).unwrap();
            assert_ne!(
                got.diagnostics.strategy, "strict_scanline_fallback",
                "{:?}",
                got.diagnostics
            );
            for (bi, b) in r.bands.iter().enumerate() {
                let (mut sum, mut support) = (Sum::default(), Sum::default());
                for row in 0..r.grid.height {
                    for col in 0..r.grid.width {
                        let f = (((col + 1) as f64).min(x1) - (col as f64).max(x0)).max(0.);
                        let f = f * (((row + 1) as f64).min(y1) - (row as f64).max(y0)).max(0.);
                        let cell = row * r.grid.width + col;
                        if b.valid[cell] {
                            sum.add(f * b.values[cell]);
                            support.add(f);
                        }
                    }
                }
                close(got.bands[bi].fractional_sum, sum.value());
                close(got.bands[bi].covered_cell_equivalents, support.value());
            }
        }
    }
}

#[test]
fn architecture_cumulative_arbitrary_holes_clipping_orientation_and_components() {
    let r = raster(80, 64);
    let c = AtomicBool::new(false);
    let mut rings = vec![
        vec![
            [-2.25, 7.375],
            [67.125, 1.25],
            [78.75, 58.625],
            [18.375, 69.25],
            [-2.25, 7.375],
        ],
        vec![
            [24.25, 20.375],
            [39.875, 21.625],
            [38.625, 35.125],
            [23.125, 32.875],
            [24.25, 20.375],
        ],
    ];
    let mut polys = vec![polygon(rings.clone())];
    rings.iter_mut().for_each(|r| r.reverse());
    polys.push(polygon(rings));
    polys.push(json!({"type":"MultiPolygon","coordinates":[rectangle(1.25,2.25,25.75,28.125)["coordinates"],rectangle(40.25,35.375,79.125,63.25)["coordinates"]]}));
    for p in polys {
        let expected = reference(&r, &p);
        for origin in [
            Origin::Full,
            Origin::Blocked { columns: 8 },
            Origin::Blocked { columns: 31 },
        ] {
            let got = CumulativeField::build(&r, origin, &c)
                .unwrap()
                .query(&r, &p, &c)
                .unwrap();
            assert_ne!(
                got.diagnostics.strategy, "strict_scanline_fallback",
                "{:?}",
                got.diagnostics
            );
            assert_eq!(got.diagnostics.strict_fallback_cell_visits, 0);
            for (a, b) in got.bands.iter().zip(&expected) {
                close(a.fractional_sum, b.fractional_sum);
                close(a.covered_cell_equivalents, b.covered_cell_equivalents);
                close(
                    a.coverage_weighted_mean.unwrap(),
                    b.coverage_weighted_mean.unwrap(),
                );
            }
        }
    }
}

#[test]
fn architecture_cumulative_full_prefix_cancellation_and_blocked_recovery() {
    let mut r = raster(64, 2);
    r.bands.truncate(1);
    r.bands[0].valid.fill(true);
    r.bands[0].values.fill(1.);
    for row in 0..2 {
        r.bands[0].values[row * 64] = 1e16;
    }
    // Counterexample to an unguarded rounded cumulative-field subtraction.
    assert_eq!((1e16_f64 + 1.) - 1e16, 0.);
    let c = AtomicBool::new(false);
    let p = rectangle(56.25, 0.25, 57.75, 1.75);
    let full = CumulativeField::build(&r, Origin::Full, &c)
        .unwrap()
        .query(&r, &p, &c)
        .unwrap();
    let blocked = CumulativeField::build(&r, Origin::Blocked { columns: 8 }, &c)
        .unwrap()
        .query(&r, &p, &c)
        .unwrap();
    assert_eq!(full.diagnostics.strategy, "strict_scanline_fallback");
    assert_eq!(blocked.diagnostics.strategy, "cumulative_blocked");
    close(full.bands[0].fractional_sum, 2.25);
    close(blocked.bands[0].fractional_sum, 2.25);
    assert_eq!(blocked.bands[0].coverage_weighted_mean, Some(1.));
}

#[test]
fn architecture_cumulative_tiny_mean_and_high_dynamic_signed_masked_values() {
    let mut r = raster(32, 32);
    let c = AtomicBool::new(false);
    r.bands[0].values[0] = 1e18;
    r.bands[0].valid[0] = false;
    r.bands[1].values[66] = 1e15;
    r.bands[1].values[67] = -1e15;
    for p in [
        rectangle(2.25, 2.25, 20.125, 20.875),
        rectangle(3.25, 3.25, 3.25 + 1e-12, 3.75),
    ] {
        let expected = reference(&r, &p);
        for origin in [Origin::Full, Origin::Blocked { columns: 8 }] {
            let got = CumulativeField::build(&r, origin, &c)
                .unwrap()
                .query(&r, &p, &c)
                .unwrap();
            if p["coordinates"][0][1][0].as_f64().unwrap()
                - p["coordinates"][0][0][0].as_f64().unwrap()
                < 1e-4
            {
                assert_eq!(got.diagnostics.strategy, "strict_scanline_fallback");
            }
            for (a, b) in got.bands.iter().zip(&expected) {
                close(a.fractional_sum, b.fractional_sum);
                close(a.covered_cell_equivalents, b.covered_cell_equivalents);
                if let (Some(a), Some(b)) = (a.coverage_weighted_mean, b.coverage_weighted_mean) {
                    close(a, b);
                }
            }
        }
    }
}

#[test]
fn architecture_cumulative_selection_empty_mask_identity_and_cancellation() {
    let mut r = raster(16, 12);
    r.bands[1].valid.fill(false);
    let c = AtomicBool::new(false);
    let ix = CumulativeField::build_selected(&r, &[1], Origin::Full, &c).unwrap();
    let got = ix
        .query(&r, &rectangle(1.25, 1.25, 14.75, 10.75), &c)
        .unwrap();
    assert_eq!(got.bands.len(), 1);
    assert_eq!(got.bands[0].band, 1);
    assert_eq!(got.bands[0].fractional_sum, 0.);
    assert_eq!(got.bands[0].coverage_weighted_mean, None);
    let got = ix
        .query(&r, &json!({"type":"Polygon","coordinates":[]}), &c)
        .unwrap();
    assert_eq!(got.bands[0].covered_cell_equivalents, 0.);
    assert!(CumulativeField::build_selected(&r, &[0, 0], Origin::Full, &c).is_err());
    assert!(CumulativeField::build(&r, Origin::Blocked { columns: 0 }, &c).is_err());
    assert!(CumulativeField::build(&r, Origin::Blocked { columns: 64 }, &c).is_ok());
    assert!(
        ix.query(&r, &rectangle(0., 0., 1., 1.), &AtomicBool::new(true))
            .is_err()
    );
    assert!(CumulativeField::build(&r, Origin::Full, &AtomicBool::new(true)).is_err());
    r.source_id = "changed".into();
    assert!(ix.query(&r, &rectangle(0., 0., 1., 1.), &c).is_err());
}

#[test]
fn architecture_cumulative_query_subset_and_support_only_skip_value_fields() {
    let r = raster(32, 32);
    let c = AtomicBool::new(false);
    let ix = CumulativeField::build(&r, Origin::Full, &c).unwrap();
    assert_eq!(
        ix.bytes,
        CumulativeField::estimate_bytes(&r, &[0, 1, 2], Origin::Full).unwrap()
    );
    let p = rectangle(1.25, 2.25, 29.75, 30.5);
    let all = ix.query(&r, &p, &c).unwrap();
    let subset = ix.query_selected(&r, &p, &[2], true, &c).unwrap();
    let support = ix.query_selected(&r, &p, &[2], false, &c).unwrap();
    assert_eq!(subset.bands.len(), 1);
    assert_eq!(subset.bands[0].band, 2);
    close(subset.bands[0].fractional_sum, all.bands[2].fractional_sum);
    close(
        support.bands[0].covered_cell_equivalents,
        all.bands[2].covered_cell_equivalents,
    );
    assert_eq!(support.diagnostics.boundary_value_reads, 0);
    assert!(support.diagnostics.boundary_mask_reads > 0);
    assert_eq!(support.bands[0].coverage_weighted_mean, None);
    assert_eq!(
        subset.diagnostics.prefix_reads * 3,
        all.diagnostics.prefix_reads
    );
    assert_eq!(
        support.diagnostics.prefix_reads * 2,
        subset.diagnostics.prefix_reads
    );
    assert!(ix.query_selected(&r, &p, &[2, 2], true, &c).is_err());
    assert!(ix.query_selected(&r, &p, &[3], true, &c).is_err());
}

#[test]
fn architecture_cumulative_nonidentity_affine_and_nearly_coincident_crossings() {
    let mut r = raster(24, 24);
    let c = AtomicBool::new(false);
    r.grid.transform = [1e6, 0.003, 0., 3e6, 0., -0.007];
    for ring in [
        vec![
            [1.125, 2.25],
            [20.125, 3.375],
            [19.625, 21.75],
            [2.25, 19.375],
            [1.125, 2.25],
        ],
        vec![[1., 1.], [21., 21. + 1e-12], [20., 22.], [1., 2.], [1., 1.]],
    ] {
        let p = json!({"type":"Polygon","coordinates":[ring.iter().map(|p|[r.grid.transform[0]+p[0]*r.grid.transform[1],r.grid.transform[3]+p[1]*r.grid.transform[5]]).collect::<Vec<_>>() ]});
        let expected = reference(&r, &p);
        for origin in [Origin::Full, Origin::Blocked { columns: 7 }] {
            let got = CumulativeField::build(&r, origin, &c)
                .unwrap()
                .query(&r, &p, &c)
                .unwrap();
            for (a, b) in got.bands.iter().zip(&expected) {
                close(a.fractional_sum, b.fractional_sum);
                close(a.covered_cell_equivalents, b.covered_cell_equivalents);
            }
        }
    }
}
