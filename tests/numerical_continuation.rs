use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};
use raster_engine::{
    aggregate::{self, Options},
    coverage::{Cell, Plan},
    model::{Band, Grid, Raster},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

fn raster(values: &[f64], valid: &[bool], weights: Option<&[f64]>) -> Raster {
    let mut bands = vec![Band {
        values: values.to_vec(),
        valid: valid.to_vec(),
        unit: Some("native".into()),
    }];
    if let Some(w) = weights {
        bands.push(Band {
            values: w.to_vec(),
            valid: vec![true; w.len()],
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
        source_id: "reducer-fixture".into(),
    }
}
fn plan(r: &Raster, fractions: &[f64]) -> Plan {
    Plan {
        grid: r.grid.clone(),
        grid_id: r.grid.identity(),
        identity: "independent-fractions".into(),
        spans: vec![],
        cells: fractions
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0.)
            .map(|(i, &f)| Cell {
                row: 0,
                col: i,
                fraction: f,
            })
            .collect(),
        selected: fractions.iter().sum(),
        polygon_area: fractions.iter().sum(),
        intersecting: fractions.iter().filter(|f| **f > 0.).count(),
        strategy: "independent-fraction-fixture".into(),
        validation_ms: 0.,
        compilation_ms: 0.,
    }
}
fn options(stats: &[&str]) -> Options {
    Options {
        bands: vec![0],
        statistics: Some(stats.iter().map(|s| s.to_string()).collect()),
        ..Options::default()
    }
}
fn measure(r: &Raster, f: &[f64], o: &Options) -> anyhow::Result<Value> {
    let answer = aggregate::measure(r, &plan(r, f), o, None, &AtomicBool::new(false))?;
    Ok(serde_json::to_value(&answer[0])?)
}
fn exact_variance(v: &[f64], f: &[f64], valid: &[bool], weights: Option<&[f64]>) -> f64 {
    let rows: Vec<_> = v
        .iter()
        .zip(f)
        .zip(valid)
        .enumerate()
        .filter(|(_, ((_, f), valid))| **valid && **f > 0.)
        .filter_map(|(i, ((&v, &f), _))| {
            let mut weight = BigRational::from_float(f).unwrap();
            if let Some(w) = weights {
                if w[i] < 0. {
                    return None;
                };
                weight *= BigRational::from_float(w[i]).unwrap();
            }
            Some((BigRational::from_float(v).unwrap(), weight))
        })
        .collect();
    let w: BigRational = rows.iter().map(|(_, w)| w).sum();
    if w.is_zero() {
        return 0.;
    };
    let mean = rows.iter().map(|(v, w)| v * w).sum::<BigRational>() / &w;
    (rows
        .iter()
        .map(|(v, w)| {
            let d = v - &mean;
            &d * &d * w
        })
        .sum::<BigRational>()
        / w)
        .to_f64()
        .unwrap()
}
fn close_variance(actual: f64, expected: f64) {
    if expected == 0. {
        assert_eq!(actual, 0.);
        return;
    }
    let ulp = expected - expected.next_down();
    assert!(
        actual > 0. && (actual - expected).abs() <= 1e-10 * expected.abs() + 8. * ulp,
        "{actual} != {expected}"
    );
}

#[test]
fn population_variance_is_fractional_stable_and_masked() {
    for (v, f) in [
        (vec![1., 5.], vec![0.25, 0.75]),
        (
            vec![1e16, 1e16 + 2., 1e16 + 4., 1e16 + 6.],
            vec![1., 0.5, 0.25, 0.125],
        ),
        (
            vec![1e16 + 6., 1e16 + 4., 1e16 + 2., 1e16],
            vec![0.125, 0.25, 0.5, 1.],
        ),
        (vec![-3., -1., 4.], vec![1e-300, 2e-300, 3e-300]),
        (vec![1e-100, 3e-100], vec![1e-300, 1e-300]),
        (vec![0., 1e160], vec![1., 1e-320]),
        (vec![0., 1e-160], vec![1., 1.]),
        (
            vec![7., 7., 7.],
            vec![f64::from_bits(1), f64::from_bits(2), f64::from_bits(3)],
        ),
    ] {
        let valid = vec![true; v.len()];
        let r = raster(&v, &valid, None);
        let result = measure(&r, &f, &options(&["variance", "stddev"])).unwrap();
        let expected = exact_variance(&v, &f, &valid, None);
        close_variance(result["variance"].as_f64().unwrap(), expected);
        let expected_stddev = if expected.is_subnormal() && v.len() == 2 && f[0] == f[1] {
            // Independent analytic equal-weight two-point deviation; do not
            // take sqrt after prematurely rounding a subnormal variance.
            (v[1] - v[0]).abs() / 2.
        } else {
            expected.sqrt()
        };
        close_variance(result["stddev"].as_f64().unwrap(), expected_stddev);
    }
    let r = raster(&[-1000., 1., 5.], &[false, true, true], None);
    let result = measure(&r, &[1., 0.25, 0.75], &options(&["variance", "stddev"])).unwrap();
    close_variance(result["variance"].as_f64().unwrap(), 3.);
    assert_eq!(result["valid_cell_count"], 2);
}

#[test]
fn varied_large_offset_moments_match_exact_reference_in_both_orders() {
    let mut state = 0x123456789abcdefu64;
    for case in 0..24 {
        let base = if case % 3 == 0 {
            1e16
        } else if case % 3 == 1 {
            -1e12
        } else {
            0.
        };
        let mut v = Vec::new();
        let mut f = Vec::new();
        let mut valid = Vec::new();
        for _ in 0..97 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.push(base + ((state >> 32) % 31) as f64 * 2.);
            f.push(2f64.powi(-(((state >> 24) % 30) as i32)));
            valid.push(state % 11 != 0);
        }
        let expected = exact_variance(&v, &f, &valid, None);
        for _ in 0..2 {
            let result = measure(&raster(&v, &valid, None), &f, &options(&["variance"])).unwrap();
            close_variance(result["variance"].as_f64().unwrap(), expected);
            v.reverse();
            f.reverse();
            valid.reverse();
        }
    }
}

#[test]
fn joint_weights_do_not_change_unweighted_variance() {
    let v = [1., 5., 11., -3.];
    let f = [0.25, 0.75, 1., 0.5];
    let w = [4., 0., -2., 3.];
    let valid = [true; 4];
    let r = raster(&v, &valid, Some(&w));
    let mut o = options(&["variance", "weighted_variance", "weighted_stddev"]);
    o.weight_band = Some(1);
    let result = measure(&r, &f, &o).unwrap();
    close_variance(
        result["variance"].as_f64().unwrap(),
        exact_variance(&v, &f, &valid, None),
    );
    close_variance(
        result["weighted_variance"].as_f64().unwrap(),
        exact_variance(&v, &f, &valid, Some(&w)),
    );
    let r = raster(&[1., 5.], &[true, true], Some(&[0., 0.]));
    let result = measure(&r, &[0.25, 0.75], &o).unwrap();
    assert!(result["weighted_variance"].is_null());
    close_variance(result["variance"].as_f64().unwrap(), 3.);
}

#[test]
fn categories_have_closed_domains_and_exact_tie_ordering() {
    let mut o = options(&["categories", "majority", "variety"]);
    o.category_values = Some(vec![10., 20., 30.]);
    let r = raster(&[10., 20., 20.], &[true; 3], None);
    let answer = measure(&r, &[0.5, 0.5, 2f64.powi(-54)], &o).unwrap();
    assert_eq!(answer["majority"], 20.);
    assert_eq!(answer["variety"], 2);
    let tie = measure(&r, &[0.5, 0.25, 0.25], &o).unwrap();
    assert_eq!(tie["majority"], 10.);
    assert_eq!(tie["categories"]["fractions"], json!([0.5, 0.5, 0.]));
    let bad = raster(&[40.], &[true], None);
    assert!(
        measure(&bad, &[1.], &o)
            .unwrap_err()
            .to_string()
            .contains("outside category_values")
    );
    let absent = raster(&[40.], &[false], None);
    let empty = measure(&absent, &[1.], &o).unwrap();
    assert_eq!(empty["variety"], 0);
    assert!(empty["majority"].is_null());
    assert_eq!(empty["categories"]["fractions"], json!([null, null, null]));
    let mut zero = options(&["categories", "majority"]);
    zero.category_values = Some(vec![-0., 1.]);
    assert_eq!(
        measure(&raster(&[0., -0.], &[true, true], None), &[0.5, 0.5], &zero).unwrap()["majority"],
        0.
    );
}

#[test]
fn bounded_quantiles_use_exact_inverse_cdf_and_joint_validity() {
    let mut o = options(&["median", "quantiles"]);
    o.quantiles = Some(vec![0., 0.25, 0.5, 1.]);
    let r = raster(&[1., 5.], &[true, true], None);
    let answer = measure(&r, &[0.25, 0.75], &o).unwrap();
    assert_eq!(answer["median"], 5.);
    assert_eq!(answer["quantiles"]["values"], json!([1., 1., 5., 5.]));
    // Rounded total/cumulative support would return the wrong lower category.
    let r = raster(&[0., 1., 1.], &[true; 3], None);
    assert_eq!(
        measure(&r, &[0.5, 0.5, 2f64.powi(-54)], &o).unwrap()["median"],
        1.
    );
    let mut weighted = options(&["weighted_median", "weighted_quantiles"]);
    weighted.quantiles = Some(vec![0., 0.5, 1.]);
    weighted.weight_band = Some(1);
    let r = raster(&[1., 5., 9.], &[true; 3], Some(&[4., 0., -1.]));
    assert_eq!(
        measure(&r, &[0.25, 0.75, 1.], &weighted).unwrap()["weighted_quantiles"]["values"],
        json!([1., 1., 1.])
    );
    // Exact rank does not multiply two tiny positive binary64 inputs into zero.
    let r = raster(&[1., 5.], &[true, true], Some(&[1e-300, 2e-300]));
    assert_eq!(
        measure(&r, &[1e-300, 1e-300], &weighted).unwrap()["weighted_median"],
        5.
    );
    o.quantile_max_samples = Some(1);
    assert!(
        measure(&r, &[1., 1.], &o)
            .unwrap_err()
            .to_string()
            .contains("sample budget")
    );
}

#[test]
fn new_fields_are_explicit_and_invalid_options_fail() {
    let r = raster(&[1., 5.], &[true, true], None);
    let legacy = measure(&r, &[1., 1.], &Options::default()).unwrap();
    for key in [
        "variance",
        "stddev",
        "categories",
        "majority",
        "variety",
        "median",
        "quantiles",
    ] {
        assert!(legacy.get(key).is_none(), "{key}");
    }
    let absent = raster(&[1., 5.], &[false, false], None);
    assert!(measure(&absent, &[1., 1.], &options(&["variance"])).unwrap()["variance"].is_null());
    for mut invalid in [
        options(&["categories"]),
        options(&["quantiles"]),
        options(&["weighted_variance"]),
    ] {
        assert!(invalid.validate(1).is_err());
        invalid.statistics = None;
        invalid.quantile_max_samples = Some(1);
        assert!(invalid.validate(1).is_err());
    }
    let mut invalid = options(&["categories"]);
    invalid.category_values = Some(vec![-0., 0.]);
    assert!(invalid.validate(1).is_err());
    invalid.category_values = Some(vec![f64::NAN]);
    assert!(invalid.validate(1).is_err());
    let mut invalid = options(&["quantiles"]);
    invalid.quantiles = Some(vec![0.5, 0.5]);
    assert!(invalid.validate(1).is_err());
    assert!(!options(&["variance"]).summaries_eligible());
}

#[test]
fn unrepresentable_positive_moment_cannot_be_erased_during_rescale() {
    // Exact variance is positive (~5e-331), although its binary64 rounding
    // is zero. A former scaled-M2 underflow incorrectly made this appear
    // constant after the heavier third contribution.
    let v = [0., 1e-160, 5e-161];
    let f = [1e-10, 1e-10, 1.];
    let error = measure(&raster(&v, &[true; 3], None), &f, &options(&["variance"])).unwrap_err();
    assert!(error.to_string().contains("below binary64 range"));
    let error = measure(
        &raster(&[0., 1e-200], &[true; 2], None),
        &[1., 1.],
        &options(&["variance"]),
    )
    .unwrap_err();
    assert!(error.to_string().contains("below binary64 range"));
}

#[test]
fn extreme_effective_weights_preserve_finite_variance_via_bounded_fallback() {
    let mut o = options(&["weighted_variance"]);
    o.weight_band = Some(1);
    for (values, fractions, weights, fallback) in [
        (vec![0., 1e160], vec![1., 1.], vec![1., 1e-320], false),
        (vec![0., 1e160], vec![1., 0.3], vec![1., 1e-320], true),
        (vec![0., 1e160], vec![1., 1.], vec![1e308, 1e-12], true),
        (
            vec![1., 5.],
            vec![1e-200, 1e-200],
            vec![1e-200, 1e-200],
            true,
        ),
        (vec![-1e308, 1e308], vec![1., 1.], vec![1e-320, 1.], true),
    ] {
        let r = raster(&values, &[true; 2], Some(&weights));
        let answer = measure(&r, &fractions, &o).unwrap();
        close_variance(
            answer["weighted_variance"].as_f64().unwrap(),
            exact_variance(&values, &fractions, &[true; 2], Some(&weights)),
        );
        assert_eq!(
            answer["moment_diagnostics"]["weighted_rational_fallback"],
            fallback
        );
    }
}

#[test]
fn stddev_rounds_only_the_requested_final_value() {
    let tiny = f64::from_bits(1);
    for (values, expected) in [
        ([0., 1e-200], 5e-201),
        ([0., 1e-160], 5e-161),
        ([-1e200, 1e200], 1e200),
        ([-f64::MAX, f64::MAX], f64::MAX),
        ([-tiny, tiny], tiny),
    ] {
        let r = raster(&values, &[true; 2], Some(&[1., 1.]));
        let mut o = options(&["stddev", "weighted_stddev"]);
        o.weight_band = Some(1);
        let result = measure(&r, &[1., 1.], &o).unwrap();
        close_variance(result["stddev"].as_f64().unwrap(), expected);
        close_variance(result["weighted_stddev"].as_f64().unwrap(), expected);
        assert!(result.get("variance").is_none());
    }
    let error = measure(
        &raster(&[0., tiny], &[true; 2], None),
        &[1., 1.],
        &options(&["stddev"]),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("standard deviation is below binary64 range")
    );
}
