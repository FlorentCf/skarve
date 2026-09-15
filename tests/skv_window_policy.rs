//! Wider source-layout planning preserves native data and conservative bounds.
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    batch::{BorrowedSource, Job, JobSpec},
    io::open_source,
    skv::{CompileOptions, SkvSource, compile},
    source::WindowSource,
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

const WIDTH: usize = 513;
const HEIGHT: usize = 385;
const BANDS: usize = 40;

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]})
}
fn zones() -> Vec<Value> {
    let mut hole = rectangle(1.25, 1.5, 507.75, 380.25);
    hole["coordinates"].as_array_mut().unwrap().push(json!([
        [135.25, 129.5],
        [135.25, 261.25],
        [267.75, 261.25],
        [267.75, 129.5],
        [135.25, 129.5]
    ]));
    [
        rectangle(0.25, 0.5, 512.75, 384.75),
        rectangle(253.125, 1.5, 253.12500001, 383.25),
        hole,
        rectangle(-20.5, -10.25, 20.75, 35.5),
        rectangle(600., 450., 620., 475.),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, g)| json!({"id":format!("zone-{i}"),"version":"1","geometry":g}))
    .collect()
}
fn spec(policy: &str, tile_bytes: usize, slices: &[&str]) -> JobSpec {
    serde_json::from_value(json!({
        "zones":zones(),
        "slices":slices.iter().map(|s| json!({"id":s,"source":s})).collect::<Vec<_>>(),
        "crs":"EPSG:3857","tile_edge":128,"window_policy":policy,
        "options":{"bands":(0..BANDS).map(|i|i*17%BANDS).collect::<Vec<_>>(),
            "statistics":["sum","support","mean","min","max","count"]},
        "budget":{"working_bytes":1usize<<30,"geometry_bytes":128<<20,
            "tile_bytes":tile_bytes,"decoded_bytes":2usize<<30}
    }))
    .unwrap()
}
fn batch(source: &dyn WindowSource, policy: &str, tile_bytes: usize) -> Value {
    Job::new(spec(policy, tile_bytes, &["data"]), None, 4096, 1 << 30)
        .unwrap()
        .next(
            zones().len(),
            |_| Ok(Box::new(BorrowedSource(source))),
            &AtomicBool::new(false),
        )
        .unwrap()
}
fn strict_equal(actual: &Value, expected: &Value, at: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                e.keys().collect::<Vec<_>>(),
                "{at}"
            );
            for (k, v) in e {
                strict_equal(&a[k], v, &format!("{at}.{k}"));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "{at}");
            for (i, (a, e)) in a.iter().zip(e).enumerate() {
                strict_equal(a, e, &format!("{at}[{i}]"));
            }
        }
        (Value::Number(a), Value::Number(e)) if a.is_f64() && e.is_f64() => {
            let (a, e) = (a.as_f64().unwrap(), e.as_f64().unwrap());
            assert!(
                a.is_finite() && e.is_finite() && (a - e).abs() <= 1e-8 + 1e-10 * e.abs(),
                "{at}: {a} != {e}"
            );
        }
        _ => assert_eq!(actual, expected, "{at}"),
    }
}

#[test]
fn forty_band_skv_raw_batch_uses_physical_edge_only_when_all_bounds_fit() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original.tif");
    let mut dataset = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&original, WIDTH, HEIGHT, BANDS)
        .unwrap();
    dataset
        .set_geo_transform(&[0., 1., 0., HEIGHT as f64, 0., -1.])
        .unwrap();
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for b in 0..BANDS {
        let values = (0..WIDTH * HEIGHT)
            .map(|i| {
                if (i + b) % 79 == 0 {
                    -9999.
                } else {
                    ((i % 251) as f32 - 125.) * 0.25 + b as f32
                }
            })
            .collect();
        let mut band = dataset.rasterband(b + 1).unwrap();
        band.write(
            (0, 0),
            (WIDTH, HEIGHT),
            &mut Buffer::new((WIDTH, HEIGHT), values),
        )
        .unwrap();
        band.set_no_data_value(Some(-9999.)).unwrap();
        band.set_scale(1.25).unwrap();
        band.set_offset(-(b as f64)).unwrap();
        band.create_mask_band(false).unwrap();
        let mask = (0..WIDTH * HEIGHT)
            .map(|i| [0u8, 1, 127, 255][(i + b) % 4])
            .collect();
        band.open_mask_band()
            .unwrap()
            .write(
                (0, 0),
                (WIDTH, HEIGHT),
                &mut Buffer::new((WIDTH, HEIGHT), mask),
            )
            .unwrap();
    }
    dataset.flush_cache().unwrap();
    drop(dataset);
    let cancel = AtomicBool::new(false);
    let original_source = open_source(
        &serde_json::from_value(json!({"location":original})).unwrap(),
        &cancel,
    )
    .unwrap();
    assert_eq!(original_source.max_read_bands(), 20);
    for edge in [128, 256] {
        let path = directory.path().join(format!("edge-{edge}.skv"));
        let options: CompileOptions = serde_json::from_value(json!({
            "chunk_edge":edge,"band_group":40,"codec":"deflate","predictor":"byte_delta_v1",
            "payload_layout":"band","summaries":true,"working_bytes":128<<20
        }))
        .unwrap();
        compile(
            original_source.as_ref(),
            path.to_str().unwrap(),
            &options,
            &cancel,
        )
        .unwrap();
    }
    drop(original_source);
    std::fs::remove_file(original).unwrap(); // Serving must not need the original.
    let sources =
        [128, 256].map(|edge| {
            SkvSource::open(&serde_json::from_value(json!({
        "location":directory.path().join(format!("edge-{edge}.skv")),"use_summaries":false
    })).unwrap(),&cancel).unwrap()
        });
    let fixed = batch(&sources[1], "fixed", 256 << 20);
    let planned = batch(&sources[1], "source_layout", 256 << 20);
    strict_equal(&planned["rows"], &fixed["rows"], "rows");
    assert_eq!(planned["complete"], true);
    assert_eq!(planned["rows"].as_array().unwrap().len(), zones().len());
    for row in planned["rows"].as_array().unwrap() {
        assert_eq!(row["bands"].as_array().unwrap().len(), BANDS);
    }
    let metrics = &planned["metrics"];
    assert_eq!(metrics["last_window_policy"]["executed_edge"], 256);
    assert_eq!(metrics["window_policy_promotions"], 1);
    assert!(
        metrics["windows_read"].as_u64().unwrap()
            < fixed["metrics"]["windows_read"].as_u64().unwrap()
    );
    for key in [
        "source_summary_records_read",
        "summary_records_read",
        "summarized_polygon_tiles",
    ] {
        assert_eq!(metrics[key], 0, "{key}");
    }
    assert!(metrics["peak_tracked_bytes"].as_u64().unwrap() <= 1 << 30);
    let limited = batch(&sources[1], "source_layout", 64 << 20);
    strict_equal(&limited["rows"], &fixed["rows"], "limited");
    assert_eq!(
        limited["metrics"]["last_window_policy"]["executed_edge"],
        128
    );
    assert_eq!(limited["metrics"]["window_policy_promotions"], 0);

    // Equal grid identity with different physical edges must not reuse the first
    // slice's geometry. A third identical layout can reuse it safely.
    let mut job = Job::new(
        spec(
            "source_layout",
            256 << 20,
            &["large", "small", "large-again"],
        ),
        None,
        4096,
        1 << 30,
    )
    .unwrap();
    let mut pages = Vec::new();
    for _ in 0..3 {
        pages.push(
            job.next(
                zones().len(),
                |id| {
                    Ok(Box::new(BorrowedSource(if id.id == "small" {
                        &sources[0]
                    } else {
                        &sources[1]
                    })))
                },
                &cancel,
            )
            .unwrap(),
        );
    }
    assert_eq!(
        pages[0]["metrics"]["last_window_policy"]["executed_edge"],
        256
    );
    assert_eq!(
        pages[1]["metrics"]["last_window_policy"]["executed_edge"],
        128
    );
    assert_eq!(pages[1]["metrics"]["geometry_cache_hits"], 0);
    assert_eq!(pages[2]["metrics"]["geometry_cache_hits"], zones().len());
    assert_eq!(pages[2]["complete"], true);
    for page in &pages {
        for (a, e) in page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .zip(fixed["rows"].as_array().unwrap())
        {
            strict_equal(&a["bands"], &e["bands"], "cross-layout bands");
            assert_eq!(a["zone_id"], e["zone_id"]);
        }
    }
    let before = sources[1].diagnostics();
    let cancelled = Job::new(
        spec("source_layout", 256 << 20, &["data"]),
        None,
        4096,
        1 << 30,
    )
    .unwrap()
    .next(
        zones().len(),
        |_| Ok(Box::new(BorrowedSource(&sources[1]))),
        &AtomicBool::new(true),
    );
    assert!(cancelled.is_err());
    assert_eq!(
        sources[1].diagnostics(),
        before,
        "pre-cancelled job must not read source values"
    );
}
