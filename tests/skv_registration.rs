//! Initial SKV registration is one validated range, not a removed query guard.
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    io::{open_source, open_source_for_compile},
    ordered_source::{OrderedRequest, execute},
    skv::{BOOTSTRAP, CompileOptions, compile},
    source::SourceSpec,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MISSING_TAG: u8 = 1;
const WEAK_TAG: u8 = 2;
const FULL_RESPONSE: u8 = 3;
const BAD_RANGE: u8 = 4;
const BAD_TOTAL: u8 = 5;
const BAD_LENGTH: u8 = 6;
const SHORT_BODY: u8 = 7;
const ENCODED: u8 = 8;
const HELD_BODY: u8 = 9;
const CHANGED_FINAL_GUARD: u8 = 10;
const CHANGED_FIRST_GUARD: u8 = 11;
const REDIRECT: u8 = 12;

struct Server {
    url: String,
    fault: Arc<AtomicU8>,
    requests: Arc<Mutex<Vec<String>>>,
    observed: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>, mode: u8) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/registration.skv", listener.local_addr().unwrap());
        let fault = Arc::new(AtomicU8::new(mode));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, seen, gate, done, log) = (
            fault.clone(),
            observed.clone(),
            release.clone(),
            stop.clone(),
            requests.clone(),
        );
        let worker = thread::spawn(move || {
            let mut heads = 0;
            while !done.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = Vec::new();
                let mut byte = [0];
                while !input.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    input.push(byte[0]);
                    assert!(input.len() <= 16_384);
                }
                let request = String::from_utf8(input).unwrap().to_ascii_lowercase();
                let initial = log.lock().unwrap().is_empty();
                log.lock().unwrap().push(request.clone());
                let mode = f.load(Ordering::Acquire);
                if request.starts_with("head ") {
                    heads += 1;
                    let changed =
                        mode == CHANGED_FIRST_GUARD || (mode == CHANGED_FINAL_GUARD && heads == 2);
                    let tag = if changed {
                        "changed"
                    } else {
                        "registration-v1"
                    };
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"{tag}\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                assert!(request.starts_with("get "));
                assert!(request.contains("accept-encoding: identity"));
                if !initial {
                    assert!(request.contains("if-match: \"registration-v1\""));
                }
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (a, b) = range.trim().split_once('-').unwrap();
                let start = a.parse::<usize>().unwrap();
                let end = b.parse::<usize>().unwrap().min(bytes.len() - 1);
                assert!(start <= end);
                let length = end - start + 1;
                if initial && mode == FULL_RESPONSE {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"registration-v1\"\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                if initial && mode == REDIRECT {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 302 Found\r\nLocation: /redirected.skv\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    continue;
                }
                let tag = if initial && mode == MISSING_TAG {
                    ""
                } else if initial && mode == WEAK_TAG {
                    "ETag: W/\"registration-v1\"\r\n"
                } else {
                    "ETag: \"registration-v1\"\r\n"
                };
                let encoding = if initial && mode == ENCODED {
                    "Content-Encoding: gzip\r\n"
                } else {
                    ""
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n{tag}{encoding}Connection: close\r\n\r\n",
                    length + usize::from(initial && mode == BAD_LENGTH),
                    start + usize::from(initial && mode == BAD_RANGE),
                    end,
                    if initial && mode == BAD_TOTAL {
                        0
                    } else {
                        bytes.len()
                    }
                );
                if initial && mode == HELD_BODY {
                    seen.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !gate.load(Ordering::Acquire)
                        && !done.load(Ordering::Acquire)
                        && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let sent = if initial && mode == SHORT_BODY {
                    length / 2
                } else {
                    length
                };
                let _ = socket.write_all(&bytes[start..start + sent]);
            }
        });
        Self {
            url,
            fault,
            requests,
            observed,
            release,
            stop,
            worker: Some(worker),
        }
    }
    fn spec(&self) -> SourceSpec {
        serde_json::from_value(self.spec_value()).unwrap()
    }
    fn spec_value(&self) -> serde_json::Value {
        json!({"location":self.url,"format":"skv",
            "http":{"allow_http":true,"cache_bytes":0,"max_requests":32,"timeout_seconds":3}})
    }
    fn log(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn fixture() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.tif");
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(&path, 4, 2, 1)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 2., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    ds.rasterband(1)
        .unwrap()
        .write(
            (0, 0),
            (4, 2),
            &mut Buffer::new((4, 2), vec![1f32, 2., 3., 4., 5., 6., 7., 8.]),
        )
        .unwrap();
    ds.flush_cache().unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let input = open_source_for_compile(
        &serde_json::from_value(json!({"location":path})).unwrap(),
        &cancel,
    )
    .unwrap();
    let output = dir.path().join("data.skv");
    compile(
        input.as_ref(),
        output.to_str().unwrap(),
        &CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    fs::read(output).unwrap()
}
fn request() -> OrderedRequest {
    serde_json::from_value(
        json!({"polygons":[{"id":"all","windows":[{"window":[0,0,4,2],"runs":[[0,8]]}]}]}),
    )
    .unwrap()
}

#[test]
fn registration_reuses_exact_bootstrap_once_with_cache_disabled_and_retains_query_guards() {
    let server = Server::new(fixture(), 0);
    let cancel = AtomicBool::new(false);
    let source = open_source(&server.spec(), &cancel).unwrap();
    let opened = server.log();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].starts_with("get "));
    assert!(opened[0].contains(&format!("range: bytes=0-{}", BOOTSTRAP - 1)));
    assert!(!opened[0].contains("if-match:"));
    let d = source.diagnostics();
    assert_eq!(d["remote"]["requests"], 1);
    assert_eq!(d["remote"]["head_requests"], 0);
    assert_eq!(d["remote"]["get_requests"], 1);
    assert_eq!(d["remote"]["received_bytes"], BOOTSTRAP);
    assert_eq!(d["remote"]["accepted_bytes"], BOOTSTRAP);
    assert_eq!(d["remote"]["cache_resident_bytes"], 0);
    let value = execute(source.as_ref(), &request(), &cancel).unwrap();
    assert_eq!(value["rows"][0]["bands"][0]["sum"], 36.);
    let requests = server.log();
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("head ")).count(),
        2
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .all(|r| r.contains("if-match: \"registration-v1\""))
    );
    assert_eq!(source.diagnostics()["remote"]["cache_resident_bytes"], 0);
}

#[test]
fn initial_protocol_errors_never_return_a_source_or_retry_head() {
    let bytes = fixture();
    for mode in [
        MISSING_TAG,
        WEAK_TAG,
        FULL_RESPONSE,
        BAD_RANGE,
        BAD_TOTAL,
        BAD_LENGTH,
        SHORT_BODY,
        ENCODED,
        REDIRECT,
    ] {
        let server = Server::new(bytes.clone(), mode);
        let result = open_source(&server.spec(), &AtomicBool::new(false));
        assert!(result.is_err(), "mode{mode} returned a source");
        let requests = server.log();
        assert_eq!(requests.len(), 1, "mode{mode}");
        assert!(requests[0].starts_with("get "), "mode{mode}");
    }
    let server = Server::new(vec![0u8; 128], 0);
    assert!(open_source(&server.spec(), &AtomicBool::new(false)).is_err());
    assert_eq!(server.log().len(), 1);
}

#[test]
fn declared_validator_is_sent_on_initial_range_and_pin_mismatch_fails_closed() {
    let bytes = fixture();
    for expected in ["\"registration-v1\"", "\"wrong-object\""] {
        let server = Server::new(bytes.clone(), 0);
        let mut value = server.spec_value();
        value["identity"] = json!({"sha256":format!("{:x}",Sha256::digest(&bytes)),"byte_length":bytes.len(),
            "etag":expected,"policy":"trusted_manifest"});
        let source = open_source(
            &serde_json::from_value(value).unwrap(),
            &AtomicBool::new(false),
        );
        assert_eq!(source.is_ok(), expected == "\"registration-v1\"");
        assert_eq!(server.log().len(), 1);
        assert!(server.log()[0].contains(&format!("if-match: {expected}")));
    }
}

#[test]
fn bootstrap_limits_precede_io_and_registration_spends_the_shared_budget() {
    let bytes = fixture();
    for field in ["max_range_bytes", "max_download_bytes"] {
        let server = Server::new(bytes.clone(), 0);
        let mut value = server.spec_value();
        value["http"][field] = (BOOTSTRAP - 1).into();
        assert!(
            open_source(
                &serde_json::from_value(value).unwrap(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(server.log().is_empty(), "{field}");
    }
    for field in ["max_requests", "max_download_bytes"] {
        let server = Server::new(bytes.clone(), 0);
        let mut value = server.spec_value();
        value["http"][field] = if field == "max_requests" {
            1usize
        } else {
            BOOTSTRAP
        }
        .into();
        let source = open_source(
            &serde_json::from_value(value).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let result = if field == "max_requests" {
            source.verify_immutable()
        } else {
            source
                .read_raw_selected_window_cancellable(
                    0,
                    0,
                    4,
                    2,
                    &[0],
                    16 << 20,
                    &AtomicBool::new(false),
                )
                .map(|_| ())
        };
        assert!(result.is_err(), "{field}");
        assert_eq!(server.log().len(), 1, "{field}");
    }
}

#[test]
fn cancellation_before_or_during_registration_returns_no_partial_handle() {
    let server = Server::new(fixture(), HELD_BODY);
    assert!(open_source(&server.spec(), &AtomicBool::new(true)).is_err());
    assert!(server.log().is_empty());
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let spec = server.spec();
    let worker = thread::spawn(move || {
        open_source(&spec, &worker_cancel)
            .err()
            .map(|e| format!("{e:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !server.observed.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(server.observed.load(Ordering::Acquire));
    cancel.store(true, Ordering::Release);
    server.release.store(true, Ordering::Release);
    let error = worker
        .join()
        .unwrap()
        .expect("cancelled registration must fail");
    assert!(error.contains("cancel"), "{error}");
    assert_eq!(server.log().len(), 1);
}

#[test]
fn generation_changes_at_either_query_guard_quarantine_the_opened_source() {
    for cache_bytes in [0, 4usize << 20] {
        for mode in [CHANGED_FIRST_GUARD, CHANGED_FINAL_GUARD] {
            let server = Server::new(fixture(), 0);
            let cancel = AtomicBool::new(false);
            let mut spec = server.spec_value();
            spec["http"]["cache_bytes"] = cache_bytes.into();
            let source = open_source(&serde_json::from_value(spec).unwrap(), &cancel).unwrap();
            server.fault.store(mode, Ordering::Release);
            assert!(execute(source.as_ref(), &request(), &cancel).is_err());
            assert_eq!(source.diagnostics()["invalidated"], true);
            let count = server.log().len();
            assert!(execute(source.as_ref(), &request(), &cancel).is_err());
            assert_eq!(server.log().len(), count);
            assert_eq!(
                server
                    .log()
                    .iter()
                    .filter(|r| r.starts_with("head "))
                    .count(),
                if mode == CHANGED_FIRST_GUARD { 1 } else { 2 }
            );
        }
    }
}

#[test]
fn signed_get_url_keeps_both_uncached_conditional_query_probes() {
    let server = Server::new(fixture(), 0);
    let mut spec = server.spec();
    spec.location.push_str("?isolated-test=get-only");
    let cancel = AtomicBool::new(false);
    let source = open_source(&spec, &cancel).unwrap();
    assert_eq!(server.log().len(), 1);
    assert_eq!(
        execute(source.as_ref(), &request(), &cancel).unwrap()["rows"][0]["bands"][0]["sum"],
        36.
    );
    let requests = server.log();
    assert!(requests.iter().all(|r| r.starts_with("get ")));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.lines().any(|line| line == "range: bytes=0-0"))
            .count(),
        2
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .all(|r| r.contains("if-match: \"registration-v1\""))
    );
}
