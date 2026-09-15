use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::io::{
    LocalSource, RangeSource, RemoteLimits, inspect_local, open_local, open_remote_window,
    open_window,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

fn fixture(path: &std::path::Path, transform: [f64; 6], with_crs: bool) {
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let mut ds = driver
        .create_with_band_type::<f64, _>(path, 4, 2, 2)
        .unwrap();
    ds.set_geo_transform(&transform).unwrap();
    if with_crs {
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
    }
    for i in 1..=2 {
        let mut band = ds.rasterband(i).unwrap();
        let values = if i == 1 {
            vec![-3., 0., 5., -9999., 2., f64::NAN, f64::INFINITY, 4.]
        } else {
            vec![1., 2., 3., 4., 5., 6., 7., 8.]
        };
        band.write((0, 0), (4, 2), &mut Buffer::new((4, 2), values))
            .unwrap();
        band.set_no_data_value(Some(-9999.)).unwrap();
        if i == 1 {
            band.set_scale(2.).unwrap();
            band.set_offset(1.).unwrap();
        }
        band.create_mask_band(false).unwrap();
        let mask = if i == 1 {
            vec![255, 255, 0, 255, 255, 255, 255, 255]
        } else {
            vec![0, 255, 255, 255, 255, 255, 255, 255]
        };
        band.open_mask_band()
            .unwrap()
            .write((0, 0), (4, 2), &mut Buffer::new((4, 2), mask))
            .unwrap();
    }
    ds.flush_cache().unwrap();
}

#[test]
fn local_preserves_raw_nodata_mask_scale_negative_and_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, [0., 1., 0., 2., 0., -1.], true);
    let raster = open_local(path.to_str().unwrap(), 1024 * 1024).unwrap();
    assert_eq!(raster.grid.crs, "EPSG:3857");
    assert_eq!(
        raster.bands[0].values,
        vec![-5., 1., 0., 0., 5., 0., 0., 9.]
    );
    assert_eq!(
        raster.bands[0].valid,
        vec![true, true, false, false, true, false, false, true]
    );
    assert_eq!(
        raster.bands[1].valid,
        vec![false, true, true, true, true, true, true, true]
    );
    let window = open_window(path.to_str().unwrap(), 1, 1, 2, 1, 1024 * 1024).unwrap();
    assert_eq!(window.grid.transform, [1., 1., 0., 1., 0., -1.]);
    assert_eq!(window.bands[1].values, vec![6., 7.]);
    assert_eq!(window.source_id, raster.source_id);
}

#[test]
fn registered_cache_is_lazy_band_selective_bounded_and_immutable() {
    use raster_engine::session::Session;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cached.tif");
    fixture(&path, [0., 1., 0., 2., 0., -1.], true);
    let cancel = AtomicBool::new(false);
    let mut session = Session::default();
    let registered = session
        .call(
            json!({"op":"register_file","id":"file","path":path}),
            &cancel,
        )
        .unwrap();
    assert_eq!(registered["source_opened"], false);
    session
        .call(json!({"op":"configure_file_cache","bytes":1024}), &cancel)
        .unwrap();
    let query = json!({"op":"measure_registered_file","source":"file","crs":"EPSG:3857","bands":[1],"geometry":{"type":"Polygon","coordinates":[[[0.,0.],[4.,0.],[4.,2.],[0.,2.],[0.,0.]]]}});
    let first = session.call(query.clone(), &cancel).unwrap();
    let second = session.call(query.clone(), &cancel).unwrap();
    assert_eq!(first["source_opened_this_request"], true);
    assert_eq!(second["source_opened_this_request"], false);
    assert_eq!(first["streaming"]["read_bands"], json!([1]));
    assert_eq!(first["streaming"]["decoded_value_bytes"], 64);
    assert_eq!(second["streaming"]["cache_hits"], 1);
    assert_eq!(second["streaming"]["decoded_value_bytes"], 0);
    assert_eq!(first["bands"], second["bands"]);
    assert_eq!(first["bands"][0]["band"], 1);
    assert_eq!(first["bands"][0]["fractional_sum"], 35.);
    // The requested-band order and optional weight mapping must survive compaction.
    let mut weighted = query.clone();
    weighted["bands"] = json!([1, 0]);
    weighted["weight_band"] = json!(0);
    let cached_weighted = session.call(weighted.clone(), &cancel).unwrap();
    weighted["op"] = json!("measure_file");
    weighted.as_object_mut().unwrap().remove("source");
    weighted["path"] = json!(path);
    let uncached_weighted = session.call(weighted, &cancel).unwrap();
    assert_eq!(cached_weighted["bands"], uncached_weighted["bands"]);
    session
        .call(json!({"op":"configure_file_cache","bytes":1}), &cancel)
        .unwrap();
    assert_eq!(
        session.call(json!({"op":"stats"}), &cancel).unwrap()["decoded_cache_bytes"],
        0
    );
    assert!(
        session
            .call(
                json!({"op":"configure_file_cache","bytes":134217729}),
                &cancel
            )
            .is_err()
    );
    session
        .call(json!({"op":"configure_file_cache","bytes":1024}), &cancel)
        .unwrap();
    session.call(query.clone(), &cancel).unwrap();
    // A changed source cannot be hidden behind a decoded cache hit.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(
        session
            .call(query, &cancel)
            .unwrap_err()
            .to_string()
            .contains("source changed")
    );
    let lazy_path = dir.path().join("changed-before-first-read.tif");
    fixture(&lazy_path, [0., 1., 0., 2., 0., -1.], true);
    session
        .call(
            json!({"op":"register_file","id":"lazy","path":lazy_path}),
            &cancel,
        )
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&lazy_path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let lazy_query = json!({"op":"measure_registered_file","source":"lazy","crs":"EPSG:3857","bands":[0],"geometry":{"type":"Polygon","coordinates":[]}});
    assert!(
        session
            .call(lazy_query, &cancel)
            .unwrap_err()
            .to_string()
            .contains("source changed since registration")
    );
}

#[test]
fn windows_match_whole_and_fail_before_over_budget_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path, [0., 1., 0., 2., 0., -1.], true);
    let source = LocalSource::open(path.to_str().unwrap()).unwrap();
    let full = source.read_window(0, 0, 4, 2, 1024 * 1024).unwrap();
    for y in 0..2 {
        let tile = source.read_window(0, y, 4, 1, 1024 * 1024).unwrap();
        for i in 0..2 {
            assert_eq!(
                &full.bands[i].values[y * 4..y * 4 + 4],
                &tile.bands[i].values
            );
            assert_eq!(&full.bands[i].valid[y * 4..y * 4 + 4], &tile.bands[i].valid);
        }
    }
    assert!(source.read_window(0, 0, 4, 2, 10).is_err());
    assert!(
        source
            .read_window(usize::MAX, 0, 4, 2, 1024 * 1024)
            .is_err()
    );
    assert!(source.read_window(0, 0, 0, 2, 1024 * 1024).is_err());
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let error = source
        .read_window_cancellable(0, 0, 4, 2, 1024 * 1024, &cancelled)
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    let error =
        raster_engine::io::open_local_cancellable(path.to_str().unwrap(), 1024 * 1024, &cancelled)
            .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}

#[test]
fn unsupported_grids_crs_integer_precision_and_source_mutation_fail() {
    let dir = tempfile::tempdir().unwrap();
    for (name, transform, crs) in [
        ("rotated.tif", [0., 1., 0.1, 2., 0., -1.], true),
        ("south.tif", [0., 1., 0., 0., 0., 1.], true),
        ("unknown.tif", [0., 1., 0., 2., 0., -1.], false),
    ] {
        let path = dir.path().join(name);
        fixture(&path, transform, crs);
        assert!(inspect_local(path.to_str().unwrap()).is_err());
    }
    let path = dir.path().join("integer.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<u64, _>(&path, 1, 1, 1)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 1., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    ds.rasterband(1)
        .unwrap()
        .write((0, 0), (1, 1), &mut Buffer::new((1, 1), vec![u64::MAX]))
        .unwrap();
    ds.flush_cache().unwrap();
    drop(ds);
    assert!(open_local(path.to_str().unwrap(), 1024 * 1024).is_err());
    let path = dir.path().join("source.tif");
    fixture(&path, [0., 1., 0., 2., 0., -1.], true);
    let source = LocalSource::open(path.to_str().unwrap()).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(source.read_window(0, 0, 1, 1, 1024 * 1024).is_err());
    assert!(open_local("/vsicurl/https://example.invalid/data.tif", 1024 * 1024).is_err());
}

fn mock_server(mode: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/source.tif", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut b = [0];
                conn.read_exact(&mut b).unwrap();
                bytes.push(b[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            if request.starts_with("HEAD ") {
                conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                assert!(request.to_lowercase().contains("range: bytes=10-13"));
                assert!(request.to_lowercase().contains("if-match: \"v1\""));
                let reply = match mode {
                    "ignored" => {
                        "HTTP/1.1 200 OK\r\nContent-Length: 1000000000\r\nConnection: close\r\n\r\n"
                    }
                    "changed" => {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 10-13/100\r\nETag: \"v2\"\r\nConnection: close\r\n\r\n1234"
                    }
                    "wrong_range" => {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 11-14/100\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n1234"
                    }
                    "truncated" => {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 10-13/100\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n12"
                    }
                    "missing" => {
                        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    _ => {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 10-13/100\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n1234"
                    }
                };
                let _ = conn.write_all(reply.as_bytes());
            }
        }
    });
    (url, handle)
}

#[test]
fn range_transport_checks_response_identity_integrity_and_budgets() {
    for mode in [
        "ok",
        "ignored",
        "changed",
        "wrong_range",
        "truncated",
        "missing",
    ] {
        let (url, server) = mock_server(mode);
        let mut source = RangeSource::register(
            &url,
            RemoteLimits {
                max_requests: 2,
                max_download_bytes: 4,
                max_range_bytes: 4,
                timeout_seconds: 2,
            },
        )
        .unwrap();
        assert!(source.read_range(10, 5).is_err());
        assert_eq!(source.metrics.requests, 1);
        let result = source.read_range(10, 4);
        if mode == "ok" {
            assert_eq!(result.unwrap(), b"1234");
            assert_eq!(source.metrics.accepted_bytes, 4);
        } else {
            assert!(result.is_err(), "{mode}");
            assert_eq!(source.metrics.accepted_bytes, 0);
        }
        if mode == "ignored" {
            assert_eq!(source.metrics.received_bytes, 0);
        }
        assert_eq!(source.metrics.requests, 2);
        assert!(source.read_range(10, 4).is_err());
        server.join().unwrap();
    }
}

#[test]
fn remote_cancellation_prevents_requests_and_stops_delayed_body_read() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let cancelled = Arc::new(AtomicBool::new(true));
    let error = RangeSource::register_cancellable(
        "invalid-url",
        RemoteLimits::default(),
        cancelled.clone(),
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "cancelled");
    let error = raster_engine::io::open_remote_window_cancellable(
        "invalid-url",
        0,
        0,
        1,
        1,
        1024,
        RemoteLimits::default(),
        cancelled,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/source.tif", listener.local_addr().unwrap());
    let (started, received) = mpsc::channel();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut b = [0];
                conn.read_exact(&mut b).unwrap();
                bytes.push(b[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            if bytes.starts_with(b"HEAD ") {
                conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 131072\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                conn.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 131072\r\nContent-Range: bytes 0-131071/131072\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n").unwrap();
                conn.write_all(&[1u8; 32]).unwrap();
                conn.flush().unwrap();
                started.send(()).unwrap();
                thread::sleep(std::time::Duration::from_millis(60));
                let _ = conn.write_all(&[2u8; 32]);
            }
        }
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_flag = cancel.clone();
    let canceller = thread::spawn(move || {
        received
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        worker_flag.store(true, Ordering::Relaxed);
    });
    let mut source = RangeSource::register_cancellable(
        &url,
        RemoteLimits {
            timeout_seconds: 2,
            ..Default::default()
        },
        cancel,
    )
    .unwrap();
    let error = source.read_range(0, 131072).unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert_eq!(source.metrics.requests, 2);
    assert_eq!(source.metrics.failed_requests, 1);
    assert_eq!(source.metrics.accepted_bytes, 0);
    assert!(source.metrics.received_bytes < 131072);
    canceller.join().unwrap();
    server.join().unwrap();
}

/// Python test_remote.py starts a controlled server and supplies the immutable COG URL.
#[test]
#[ignore = "requires tests/test_remote.py controlled server"]
fn remote_cog_window_from_controlled_server() {
    let url = std::env::var("RASTER_TEST_REMOTE_URL")
        .expect("start controlled server with tests/test_remote.py");
    let expected_failure = std::env::var("RASTER_TEST_REMOTE_FAILURE").is_ok();
    let result = open_remote_window(
        &url,
        257,
        259,
        16,
        12,
        2 * 1024 * 1024,
        RemoteLimits {
            max_requests: 32,
            max_download_bytes: 1024 * 1024,
            max_range_bytes: 1024 * 1024,
            timeout_seconds: 2,
        },
    );
    if expected_failure {
        assert!(result.is_err());
        return;
    }
    let (raster, metrics) = result.unwrap();
    assert_eq!(raster.grid.width, 16);
    assert_eq!(raster.grid.height, 12);
    for y in 0..12 {
        for x in 0..16 {
            let expected = ((y + 259) * 1024 + x + 257) as f64 / 16. - 1000.;
            assert_eq!(raster.bands[0].values[y * 16 + x], expected);
            assert!(raster.bands[0].valid[y * 16 + x]);
        }
    }
    assert!(metrics.accepted_bytes < 1024 * 1024);
    println!(
        "REMOTE_METRICS {}",
        serde_json::to_string(&metrics).unwrap()
    );
}

fn assert_numbers_close(actual: &serde_json::Value, expected: &serde_json::Value) {
    match (actual, expected) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!(
                (a - b).abs() <= 1e-8 + b.abs() * 1e-10,
                "{a} differs from {b}"
            );
        }
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                assert_numbers_close(a, b);
            }
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            assert_eq!(a.len(), b.len());
            for (key, b) in b {
                assert_numbers_close(&a[key], b);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

#[test]
fn streamed_matches_whole_for_multiband_masks_weights_histogram_and_outside() {
    use raster_engine::{
        aggregate::{self, Options},
        coverage, streaming,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiband.tif");
    let (width, height) = (520, 513);
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f64, _>(&path, width, height, 2)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., height as f64, 0., -1.])
        .unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for band_id in 1..=2 {
        let mut band = ds.rasterband(band_id).unwrap();
        let data = (0..width * height)
            .map(|i| {
                if band_id == 1 {
                    (i % 137) as f64 - 68.
                } else {
                    (i % 17) as f64 - 2.
                }
            })
            .collect();
        band.write(
            (0, 0),
            (width, height),
            &mut Buffer::new((width, height), data),
        )
        .unwrap();
        band.create_mask_band(false).unwrap();
        let mask = (0..width * height)
            .map(|i| {
                if i % (if band_id == 1 { 19 } else { 23 }) == 0 {
                    0u8
                } else {
                    255
                }
            })
            .collect();
        band.open_mask_band()
            .unwrap()
            .write(
                (0, 0),
                (width, height),
                &mut Buffer::new((width, height), mask),
            )
            .unwrap();
    }
    ds.flush_cache().unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let raster = open_local(path.to_str().unwrap(), 64 * 1024 * 1024).unwrap();
    let geometry = json!({"type":"Polygon","coordinates":[
        [[-10.,20.],[519.4,20.],[519.4,510.],[80.,512.],[0.,300.],[-10.,20.]],
        [[254.25,254.25],[270.75,254.25],[270.75,270.75],[254.25,270.75],[254.25,254.25]]
    ]});
    let plan = coverage::compile_polygon(&raster.grid, &geometry, "EPSG:3857", "scanline", &cancel)
        .unwrap();
    for options in [
        Options::default(),
        Options {
            bands: vec![1, 0],
            statistics: None,
            weight_band: Some(1),
            histogram_edges: Some(vec![-50., -10., 0., 10., 50.]),
            ..Default::default()
        },
    ] {
        let expected = serde_json::to_value(
            aggregate::measure(&raster, &plan, &options, None, &cancel).unwrap(),
        )
        .unwrap();
        let actual = streaming::measure_local_file(
            path.to_str().unwrap(),
            &geometry,
            "EPSG:3857",
            &options,
            &cancel,
        )
        .unwrap();
        assert_numbers_close(&actual["bands"], &expected);
        assert!(actual["streaming"]["tiles_read"].as_u64().unwrap() > 1);
        assert!(
            actual["streaming"]["max_tile_resident_bytes"]
                .as_u64()
                .unwrap()
                < 2 * 1024 * 1024
        );
        assert_eq!(actual["source_id"], raster.source_id);
    }
}

#[test]
fn streamed_empty_outside_missing_and_cancelled_are_unambiguous() {
    use raster_engine::{
        aggregate::{self, Options},
        coverage, streaming,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.tif");
    fixture(&path, [0., 1., 0., 2., 0., -1.], true);
    let raster = open_local(path.to_str().unwrap(), 1024 * 1024).unwrap();
    let options = Options {
        bands: vec![0],
        statistics: None,
        histogram_edges: Some(vec![-10., 0., 10.]),
        weight_band: Some(1),
        ..Default::default()
    };
    for (geometry, status) in [
        (json!({"type":"Polygon","coordinates":[]}), "empty"),
        (
            json!({"type":"Polygon","coordinates":[[[10.,10.],[11.,10.],[11.,11.],[10.,11.],[10.,10.]]]}),
            "outside",
        ),
        (
            json!({"type":"Polygon","coordinates":[[[2.1,1.1],[2.9,1.1],[2.9,1.9],[2.1,1.9],[2.1,1.1]]]}),
            "no_valid_data",
        ),
    ] {
        let cancel = AtomicBool::new(false);
        let plan =
            coverage::compile_polygon(&raster.grid, &geometry, "EPSG:3857", "scanline", &cancel)
                .unwrap();
        let expected = serde_json::to_value(
            aggregate::measure(&raster, &plan, &options, None, &cancel).unwrap(),
        )
        .unwrap();
        let actual = streaming::measure_local_file(
            path.to_str().unwrap(),
            &geometry,
            "EPSG:3857",
            &options,
            &cancel,
        )
        .unwrap();
        assert_numbers_close(&actual["bands"], &expected);
        assert_eq!(actual["bands"][0]["status"], status);
        if status != "no_valid_data" {
            assert_eq!(actual["streaming"]["tiles_read"], 0);
        }
    }
    let cancel = AtomicBool::new(true);
    assert!(
        streaming::measure_local_file(
            path.to_str().unwrap(),
            &json!({}),
            "EPSG:3857",
            &options,
            &cancel
        )
        .is_err()
    );
}
