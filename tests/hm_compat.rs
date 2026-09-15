use anyhow::{Result, ensure};
use raster_engine::{
    hm_compat,
    model::{Band, Grid, Raster},
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
struct MemorySource {
    meta: RasterMetadata,
    values: Vec<f64>,
    valid: Vec<bool>,
    cancel_on_read: bool,
    bad_bound: bool,
}
impl MemorySource {
    fn new(
        width: usize,
        height: usize,
        transform: [f64; 6],
        values: Vec<f64>,
        valid: Vec<bool>,
    ) -> Self {
        Self {
            meta: RasterMetadata {
                grid: Grid {
                    width,
                    height,
                    transform,
                    crs: "EPSG:4326".into(),
                },
                source_id: "hm-test".into(),
                bands: vec![BandMetadata {
                    data_type: "Float64".into(),
                    nodata: None,
                    scale: 1.,
                    offset: 0.,
                    unit: None,
                    block_size: (16, 16),
                }],
            },
            values,
            valid,
            cancel_on_read: false,
            bad_bound: false,
        }
    }
}
impl WindowSource for MemorySource {
    fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_buffer_bound(&self, w: usize, h: usize, _: &[usize]) -> Result<usize> {
        Ok(if self.bad_bound {
            usize::MAX
        } else {
            w * h * 9
        })
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        indices: &[usize],
        budget: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        ensure!(indices == [0] && w * h * 9 <= budget, "test read invalid");
        if self.cancel_on_read {
            cancel.store(true, Ordering::Relaxed);
        }
        let mut grid = self.meta.grid.clone();
        grid.width = w;
        grid.height = h;
        grid.transform[0] += x as f64 * grid.transform[1];
        grid.transform[3] += y as f64 * grid.transform[5];
        let mut values = Vec::new();
        let mut valid = Vec::new();
        for yy in y..y + h {
            for xx in x..x + w {
                let i = yy * self.meta.grid.width + xx;
                values.push(self.values[i]);
                valid.push(self.valid[i]);
            }
        }
        Ok((
            Raster {
                grid,
                bands: vec![Band {
                    values,
                    valid,
                    unit: None,
                }],
                source_id: self.meta.source_id.clone(),
            },
            ReadMetrics::default(),
        ))
    }
}
fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]})
}
fn go(s: &MemorySource, g: &Value) -> Value {
    hm_compat::measure(s, g, None, None, 4 << 20, &AtomicBool::new(false)).unwrap()
}
fn num(v: &Value, k: &str) -> f64 {
    v[k].as_f64().unwrap()
}
#[test]
fn spherical_rectangle_oracle_and_nonnegative_validity() {
    let s = MemorySource::new(1, 1, [4., 1., 0., 51., 0., -1.], vec![100.], vec![true]);
    let north = go(&s, &rect(4., 50.5, 5., 51.));
    let rad = std::f64::consts::PI / 180.;
    // Independent rectangle surface formula, without boundary edge integration.
    let f = (51. * rad).sin() - (50.5 * rad).sin();
    let f = f / ((51. * rad).sin() - (50. * rad).sin());
    assert!((num(&north, "mass") - 100. * f).abs() < 1e-10);
    assert!((num(&north, "mass") - 50.).abs() > 0.2);
    let south = go(&s, &rect(4., 50., 5., 50.5));
    assert!((num(&north, "mass") + num(&south, "mass") - 100.).abs() < 1e-10);
    assert_eq!(north["fullPixelCount"], 0);
    assert_eq!(north["boundaryPixelCount"], 1);
    assert_eq!(north["mode"], hm_compat::POLICY);
    let s = MemorySource::new(
        4,
        1,
        [4., 0.25, 0., 51., 0., -1.],
        vec![-10., 0., 20., 100.],
        vec![true, true, true, false],
    );
    let a = go(&s, &rect(4., 50., 5., 51.));
    assert_eq!(num(&a, "mass"), 20.);
    assert_eq!(num(&a, "coveredPixelEquivalent"), 4.);
    assert_eq!(num(&a, "validPopulationPixelEquivalent"), 2.);
}
#[test]
fn holes_multipart_footprint_and_thin_positive_support() {
    let s = MemorySource::new(
        2,
        2,
        [4., 0.5, 0., 51., 0., -0.5],
        vec![100.; 4],
        vec![true; 4],
    );
    let exterior = rect(3.5, 49.5, 5.5, 51.5);
    let hole = rect(4., 50., 5., 51.);
    let mut g = exterior.clone();
    g["coordinates"]
        .as_array_mut()
        .unwrap()
        .push(hole["coordinates"][0].clone());
    let a = go(&s, &g);
    assert_eq!(num(&a, "mass"), 0.);
    assert_eq!(num(&a, "footprintCoverageFraction"), 0.);
    let outside = go(&s, &rect(3.5, 50., 4.5, 51.));
    assert!((num(&outside, "footprintCoverageFraction") - 0.5).abs() < 1e-12);
    let tiny = go(&s, &rect(4.125, 50.125, 4.125 + 2f64.powi(-35), 50.875));
    assert!(num(&tiny, "mass") > 0.);
    assert!(num(&tiny, "coveredPixelEquivalent") > 0.);
    let left = rect(4., 50., 4.5, 50.5);
    let right = rect(4.5, 50.5, 5., 51.);
    let multi =
        json!({"type":"MultiPolygon","coordinates":[left["coordinates"],right["coordinates"]]});
    assert!((num(&go(&s, &multi), "mass") - 200.).abs() < 1e-10);
}
#[test]
fn summary_reuse_negative_fallback_and_version_checks() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("skarve-hm-summary-{}-{unique}", std::process::id()));
    let index = dir.join("summary.rsi");
    let mut values = vec![2.; 32 * 16];
    values[0] = -10.;
    let s = MemorySource::new(
        32,
        16,
        [4., 0.01, 0., 51., 0., -0.01],
        values,
        vec![true; 32 * 16],
    );
    let cancel = AtomicBool::new(false);
    let built = raster_engine::persistent::build_from_source(
        &s,
        dir.to_str().unwrap(),
        16,
        "band_major",
        "hierarchy",
        raster_engine::persistent::BoundarySource::Original,
        &cancel,
    )
    .unwrap();
    let g = rect(4., 50.84, 4.32, 51.);
    let direct = go(&s, &g);
    let prepared = hm_compat::measure(
        &s,
        &g,
        Some(index.to_str().unwrap()),
        built["build_id"].as_str(),
        4 << 20,
        &cancel,
    )
    .unwrap();
    for field in [
        "mass",
        "coveredPixelEquivalent",
        "validPopulationPixelEquivalent",
        "footprintCoverageFraction",
        "fullPixelCount",
        "boundaryPixelCount",
    ] {
        assert_eq!(prepared[field], direct[field], "{field}");
    }
    assert_eq!(prepared["work"]["summary_records_used"], 1);
    assert_eq!(prepared["work"]["negative_summary_fallbacks"], 1);
    assert_eq!(prepared["work"]["windows_read"], 1);
    assert!(
        hm_compat::measure(
            &s,
            &g,
            Some(index.to_str().unwrap()),
            Some("wrong"),
            4 << 20,
            &cancel
        )
        .is_err()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn rejects_capabilities_resources_and_cancel_after_read() {
    let mut s = MemorySource::new(1, 1, [4., 1., 0., 51., 0., -1.], vec![100.], vec![true]);
    let g = rect(4., 50., 5., 51.);
    let cancel = AtomicBool::new(false);
    s.bad_bound = true;
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
    s.bad_bound = false;
    s.cancel_on_read = true;
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
    s.cancel_on_read = false;
    cancel.store(false, Ordering::Relaxed);
    s.meta.grid.crs = "EPSG:3857".into();
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
    s.meta.grid.crs = "EPSG:4326".into();
    s.meta.bands[0].scale = 2.;
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
    s.meta.bands[0].scale = 1.;
    s.meta.bands[0].offset = 1.;
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
    s.meta.bands[0].offset = 0.;
    s.meta.bands.push(s.meta.bands[0].clone());
    assert!(hm_compat::measure(&s, &g, None, None, 4 << 20, &cancel).is_err());
}

#[test]
fn independent_fraction_90_digit_spherical_fixtures() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/spherical_analytical.json")).unwrap();
    let src = &fixture["sources"][0];
    let grid = &src["grid"];
    let tr: Vec<f64> = serde_json::from_value(grid["transform"].clone()).unwrap();
    let s = MemorySource::new(
        grid["width"].as_u64().unwrap() as usize,
        grid["height"].as_u64().unwrap() as usize,
        tr.try_into().unwrap(),
        serde_json::from_value(src["values"].clone()).unwrap(),
        serde_json::from_value(src["valid"].clone()).unwrap(),
    );
    for query in fixture["queries"].as_array().unwrap() {
        let actual = go(&s, &query["geometry"]);
        for (field, want) in query["expected"].as_object().unwrap() {
            let want = want.as_f64().unwrap();
            let got = actual[field].as_f64().unwrap();
            let tol = if field.ends_with("Count") {
                0.
            } else {
                1e-10 + 1e-10 * want.abs()
            };
            assert!(
                (got - want).abs() <= tol,
                "{} {field}: got{got} want{want}",
                query["id"]
            );
        }
    }
}
