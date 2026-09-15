//! Unapplied regression source for median reuse at an existing public 0.5 target.
use raster_engine::{
    aggregate::{self, Options},
    coverage::{Cell, Plan},
    model::{Band, Grid, Raster},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

const RANKS: [&str; 4] = [
    "median",
    "quantiles",
    "weighted_median",
    "weighted_quantiles",
];

fn raster(values: &[f64], valid: &[bool], weights: Option<(&[f64], &[bool])>) -> Raster {
    let mut bands = vec![Band {
        values: values.to_vec(),
        valid: valid.to_vec(),
        unit: Some("authored".into()),
    }];
    if let Some((values, valid)) = weights {
        bands.push(Band {
            values: values.to_vec(),
            valid: valid.to_vec(),
            unit: None,
        });
    }
    Raster {
        grid: Grid {
            width: values.len(),
            height: 1,
            transform: [0., 1., 0., 1., 0., -1.],
            crs: "LOCAL".into(),
        },
        bands,
        source_id: "rank-existing-median-fixture".into(),
    }
}

fn plan(r: &Raster, fractions: &[f64]) -> Plan {
    Plan {
        grid: r.grid.clone(),
        grid_id: r.grid.identity(),
        identity: "authored-rank-coverage".into(),
        spans: vec![],
        cells: fractions
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0.)
            .map(|(col, &fraction)| Cell {
                row: 0,
                col,
                fraction,
            })
            .collect(),
        selected: fractions.iter().sum(),
        polygon_area: fractions.iter().sum(),
        intersecting: fractions.iter().filter(|f| **f > 0.).count(),
        strategy: "authored-fraction-fixture".into(),
        validation_ms: 0.,
        compilation_ms: 0.,
    }
}

fn options(r: &Raster, ranks: &[&str], probabilities: &[f64]) -> Options {
    let mut statistics = vec!["sum", "support", "mean", "min", "max", "count"];
    statistics.extend_from_slice(ranks);
    if r.bands.len() == 2 {
        statistics.push("weight_sum");
    }
    Options {
        bands: vec![0],
        statistics: Some(statistics.into_iter().map(str::to_string).collect()),
        quantiles: ranks
            .iter()
            .any(|s| s.ends_with("quantiles"))
            .then(|| probabilities.to_vec()),
        weight_band: (r.bands.len() == 2).then_some(1),
        ..Options::default()
    }
}

fn measure(
    r: &Raster,
    fractions: &[f64],
    options: &Options,
    cancel: &AtomicBool,
) -> anyhow::Result<Value> {
    let answer = aggregate::measure(r, &plan(r, fractions), options, None, cancel)?;
    Ok(serde_json::to_value(&answer[0])?)
}

// JSON's ordinary numerical equality does not distinguish signed zero.
fn exact(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            assert_eq!(a.as_f64().unwrap().to_bits(), b.as_f64().unwrap().to_bits())
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                exact(a, b);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len());
            for (key, a) in a {
                exact(a, b.get(key).expect("missing output field"));
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

fn combined_matches_separate(r: &Raster, fractions: &[f64], ranks: &[&str], p: &[f64]) -> Value {
    let cancel = AtomicBool::new(false);
    let combined = measure(r, fractions, &options(r, ranks, p), &cancel).unwrap();
    let mut expected = measure(r, fractions, &options(r, &[], p), &cancel).unwrap();
    for &name in ranks {
        let single = measure(r, fractions, &options(r, &[name], p), &cancel).unwrap();
        expected
            .as_object_mut()
            .unwrap()
            .insert(name.to_string(), single[name].clone());
    }
    exact(&combined, &expected);
    for &name in ranks.iter().filter(|s| s.ends_with("quantiles")) {
        exact(&combined[name]["probabilities"], &json!(p));
        assert_eq!(combined[name]["values"].as_array().unwrap().len(), p.len());
        assert_eq!(combined[name]["method"], "inverted_cdf");
        assert_eq!(combined[name]["exact_rank"], true);
    }
    combined
}

#[test]
fn existing_half_target_preserves_separate_modes_and_independent_ranks() {
    let r = raster(&[1., 5., 9.], &[true; 3], Some((&[4., 1., 1.], &[true; 3])));
    let p = [-0., 0.25, 0.5, 0.75, 1.];
    for ranks in [&RANKS[..2], &RANKS[2..], &RANKS[..]] {
        let out = combined_matches_separate(&r, &[0.25, 0.5, 0.25], ranks, &p);
        if ranks.contains(&"median") {
            exact(&out["median"], &json!(5.));
            exact(&out["quantiles"]["values"], &json!([1., 1., 5., 5., 9.]));
        }
        if ranks.contains(&"weighted_median") {
            exact(&out["weighted_median"], &json!(1.));
            exact(
                &out["weighted_quantiles"]["values"],
                &json!([1., 1., 1., 5., 9.]),
            );
        }
    }
}

#[test]
fn exact_threshold_ties_and_tiny_products_keep_their_ranks() {
    let r = raster(&[0., 1., 1.], &[true; 3], Some((&[1.; 3], &[true; 3])));
    let out = combined_matches_separate(&r, &[0.5, 0.5, 2f64.powi(-54)], &RANKS, &[0., 0.5, 1.]);
    exact(&out["median"], &json!(1.));
    exact(&out["weighted_median"], &json!(1.));
    let tiny = raster(&[1., 5.], &[true; 2], Some((&[1e-300, 2e-300], &[true; 2])));
    let out = combined_matches_separate(&tiny, &[1e-300; 2], &RANKS, &[0., 0.5, 1.]);
    exact(&out["median"], &json!(1.));
    exact(&out["weighted_median"], &json!(5.));
}

#[test]
fn probability_signed_zero_is_preserved_and_sample_zero_is_canonical() {
    let r = raster(&[-0., 0.], &[true; 2], Some((&[1.; 2], &[true; 2])));
    let out = combined_matches_separate(&r, &[0.5; 2], &RANKS, &[-0., 0.5, 1.]);
    for name in ["median", "weighted_median"] {
        exact(&out[name], &json!(0.));
    }
    for name in ["quantiles", "weighted_quantiles"] {
        exact(&out[name]["values"], &json!([0., 0., 0.]));
        assert_eq!(
            out[name]["probabilities"][0].as_f64().unwrap().to_bits(),
            (-0f64).to_bits()
        );
    }
}

#[test]
fn empty_and_zero_or_invalid_weight_mass_do_not_borrow_other_mode() {
    let empty = raster(&[1., 5., 9.], &[false; 3], Some((&[1.; 3], &[true; 3])));
    let out = combined_matches_separate(&empty, &[1.; 3], &RANKS, &[0., 0.5, 1.]);
    for name in ["median", "weighted_median"] {
        assert!(out[name].is_null());
    }
    for name in ["quantiles", "weighted_quantiles"] {
        assert_eq!(out[name]["values"], json!([null, null, null]));
    }
    let no_weight = raster(
        &[1., 5., 9.],
        &[true; 3],
        Some((&[0., -1., 8.], &[true, true, false])),
    );
    let out = combined_matches_separate(&no_weight, &[1.; 3], &RANKS, &[0., 0.5, 1.]);
    exact(&out["median"], &json!(5.));
    assert!(out["weighted_median"].is_null());
    assert_eq!(
        out["weighted_quantiles"]["values"],
        json!([null, null, null])
    );
    let one_weight = raster(
        &[1., 5., 9.],
        &[true; 3],
        Some((&[0., -1., 8.], &[true; 3])),
    );
    let out = combined_matches_separate(&one_weight, &[1.; 3], &RANKS, &[0., 0.5, 1.]);
    exact(&out["weighted_median"], &json!(9.));
    exact(&out["weighted_quantiles"]["values"], &json!([9., 9., 9.]));
}

#[test]
fn freshly_authored_non_half_lists_keep_separate_median_path() {
    let r = raster(
        &[-17., 2., 31., 49.],
        &[true; 4],
        Some((&[1., 8., 2., 1.], &[true; 4])),
    );
    let fractions = [0.5, 0.125, 1., 0.25];
    let lists = [
        vec![0.25],
        vec![0.75],
        vec![0., 0.125, 0.875, 1.],
        (0..32).map(|i| f64::from(i) / 64.).collect(),
    ];
    for p in lists {
        assert!(!p.contains(&0.5));
        let out = combined_matches_separate(&r, &fractions, &RANKS, &p);
        // Exact masses: coverage total 1.875; weighted total 3.75.
        exact(&out["median"], &json!(31.));
        exact(&out["weighted_median"], &json!(31.));
    }
}

#[test]
fn half_only_and_single_statistic_requests_preserve_field_presence() {
    let r = raster(&[1., 9.], &[true; 2], None);
    for ranks in [&RANKS[..1], &RANKS[1..2], &RANKS[..2]] {
        let out = combined_matches_separate(&r, &[0.5; 2], ranks, &[0.5]);
        assert_eq!(out.get("median").is_some(), ranks.contains(&"median"));
        assert_eq!(out.get("quantiles").is_some(), ranks.contains(&"quantiles"));
    }
    let out = measure(&r, &[0.5; 2], &Options::default(), &AtomicBool::new(false)).unwrap();
    for name in RANKS {
        assert!(out.get(name).is_none());
    }
}

#[test]
fn existing_probability_sample_and_cancellation_guards_remain() {
    let r = raster(&[1., 9.], &[true; 2], None);
    for p in [
        vec![0.5, 0.5],
        vec![f64::NAN],
        (0..33).map(|i| f64::from(i) / 32.).collect(),
    ] {
        assert!(options(&r, &RANKS[..2], &p).validate(1).is_err());
    }
    let mut limited = options(&r, &RANKS[..2], &[0., 0.5, 1.]);
    limited.quantile_max_samples = Some(1);
    let error = measure(&r, &[1.; 2], &limited, &AtomicBool::new(false)).unwrap_err();
    assert!(error.to_string().contains("sample budget"));
    for valid in [[true; 2], [false; 2]] {
        let r = raster(&[1., 9.], &valid, None);
        let error = measure(
            &r,
            &[1.; 2],
            &options(&r, &RANKS[..2], &[0.5]),
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancel"));
    }
}
