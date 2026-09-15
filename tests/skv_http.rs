//! Controlled loopback fault cases, not WAN latency or hard-cancellation claims.
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    io::open_source_for_compile,
    skv::{CompileOptions, SkvSource, compile},
    source::{SourceSpec, WindowSource},
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MISSING_ETAG: u8 = 1;
const CHANGED_ETAG: u8 = 2;
const IGNORED_RANGE: u8 = 3;
const SHORT_BODY: u8 = 4;
const WRONG_RANGE: u8 = 5;
const HELD_BODY: u8 = 6;
const WRONG_LENGTH: u8 = 7;
struct Server {
    url: String,
    fault: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>) -> Self {
        let data_offset = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/object.skv", listener.local_addr().unwrap());
        let fault = Arc::new(AtomicU8::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let (f, done, seen, gate, count) = (
            fault.clone(),
            stop.clone(),
            observed.clone(),
            release.clone(),
            requests.clone(),
        );
        let thread = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                let request = String::from_utf8(request).unwrap();
                count.fetch_add(1, Ordering::Relaxed);
                if request.starts_with("HEAD ") {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"original-generation\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                let lower = request.to_ascii_lowercase();
                assert!(lower.starts_with("get "));
                let range = lower
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (begin, end) = range.trim().split_once('-').unwrap();
                let (begin, end) = (
                    begin.parse::<usize>().unwrap(),
                    end.parse::<usize>().unwrap(),
                );
                assert!(begin <= end && end < bytes.len());
                assert!(
                    lower.contains("if-match: \"original-generation\"")
                        || (begin == 0 && end == 16383)
                );
                let mode = if begin >= data_offset {
                    f.load(Ordering::Acquire)
                } else {
                    0
                };
                if mode == IGNORED_RANGE {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"original-generation\"\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                let length = end - begin + 1;
                let tag = if mode == MISSING_ETAG {
                    String::new()
                } else {
                    format!(
                        "ETag: \"{}\"\r\n",
                        if mode == CHANGED_ETAG {
                            "changed-generation"
                        } else {
                            "original-generation"
                        }
                    )
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n{}Connection: close\r\n\r\n",
                    length + usize::from(mode == WRONG_LENGTH),
                    begin + usize::from(mode == WRONG_RANGE),
                    end,
                    bytes.len(),
                    tag
                );
                if mode == HELD_BODY {
                    seen.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !gate.load(Ordering::Acquire)
                        && !done.load(Ordering::Acquire)
                        && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let sent = if mode == SHORT_BODY {
                    length / 2
                } else {
                    length
                };
                let _ = socket.write_all(&bytes[begin..begin + sent]);
            }
        });
        Self {
            url,
            fault,
            stop,
            observed,
            release,
            requests,
            thread: Some(thread),
        }
    }
    fn spec(&self) -> SourceSpec {
        serde_json::from_value(json!({"location":self.url,"http":{"allow_http":true,"max_requests":32,"timeout_seconds":3}})).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn bytes() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
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
    let spec: SourceSpec = serde_json::from_value(json!({"location":path})).unwrap();
    let cancel = AtomicBool::new(false);
    let input = open_source_for_compile(&spec, &cancel).unwrap();
    let output = dir.path().join("source.skv");
    compile(
        input.as_ref(),
        output.to_str().unwrap(),
        &CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    fs::read(output).unwrap()
}
#[test]
fn payload_protocol_failures_invalidate_the_reader_and_reopen_recovers() {
    let data = bytes();
    let cancel = AtomicBool::new(false);
    for (mode, expected) in [
        (MISSING_ETAG, "required remote response header missing"),
        (CHANGED_ETAG, "changed ETag"),
        (IGNORED_RANGE, "requires 206"),
        (SHORT_BODY, "body incomplete"),
        (WRONG_RANGE, "Content-Range"),
        (WRONG_LENGTH, "response length"),
    ] {
        let server = Server::new(data.clone());
        let source = SkvSource::open(&server.spec(), &cancel).unwrap();
        server.fault.store(mode, Ordering::Release);
        let error = source
            .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
            .err()
            .expect("read must fail without a partial window");
        assert!(
            error.to_string().contains(expected),
            "mode{mode}: {error:#}"
        );
        assert_eq!(source.diagnostics()["invalidated"], true);
        let requests = server.requests.load(Ordering::Relaxed);
        assert!(
            source
                .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
                .is_err()
        );
        assert_eq!(
            server.requests.load(Ordering::Relaxed),
            requests,
            "failed reader must not issue more requests"
        );
        server.fault.store(0, Ordering::Release);
        let reopened = SkvSource::open(&server.spec(), &cancel).unwrap();
        let (r, _) = reopened
            .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
            .unwrap();
        assert_eq!(r.bands[0].values, vec![1., 2., 3., 4., 5., 6., 7., 8.]);
    }
}
#[test]
fn cancellation_during_an_observed_payload_read_returns_no_window() {
    let server = Server::new(bytes());
    let cancel = Arc::new(AtomicBool::new(false));
    let source = SkvSource::open(&server.spec(), &cancel).unwrap();
    server.fault.store(HELD_BODY, Ordering::Release);
    let (seen, gate, stop) = (
        server.observed.clone(),
        server.release.clone(),
        cancel.clone(),
    );
    let cancellation = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !seen.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            seen.load(Ordering::Acquire),
            "payload request was not observed"
        );
        stop.store(true, Ordering::Release);
        gate.store(true, Ordering::Release);
    });
    let result = source.read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel);
    cancellation.join().unwrap();
    assert!(
        result
            .err()
            .expect("cancelled read must fail")
            .to_string()
            .contains("cancel")
    );
    assert!(server.observed.load(Ordering::Acquire));
    assert_eq!(source.diagnostics()["invalidated"], true);
    cancel.store(false, Ordering::Release);
    server.fault.store(0, Ordering::Release);
    let reopened = SkvSource::open(&server.spec(), &cancel).unwrap();
    assert_eq!(
        reopened
            .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
            .unwrap()
            .0
            .bands[0]
            .values
            .len(),
        8
    );
}
#[test]
fn cumulative_http_and_window_budgets_reject_without_partial_success() {
    let data = bytes();
    let cancel = AtomicBool::new(false);
    for kind in ["requests", "download", "window"] {
        let server = Server::new(data.clone());
        let mut spec = server.spec();
        if kind == "requests" {
            // Known SKV registration now consumes one GET. Leave no request budget.
            spec.http.max_requests = 1;
        }
        if kind == "download" {
            spec.http.max_download_bytes = raster_engine::skv::BOOTSTRAP as u64;
        }
        let source = SkvSource::open(&spec, &cancel).unwrap();
        let before = server.requests.load(Ordering::Relaxed);
        let max = if kind == "window" { 1 } else { 16 << 20 };
        let error = source
            .read_selected_window_cancellable(0, 0, 4, 2, &[0], max, &cancel)
            .err()
            .expect("read must fail without a partial window");
        assert!(error.to_string().contains("budget"), "{kind}: {error:#}");
        assert_eq!(source.diagnostics()["invalidated"], true);
        assert_eq!(
            server.requests.load(Ordering::Relaxed),
            before,
            "budget preflight must reject before another request"
        );
    }
}
