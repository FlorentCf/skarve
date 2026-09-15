//! Cold loopback transport evidence: only adjacent required payloads may merge.
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    aggregate::Options,
    batch::{BorrowedSource, Job, JobSpec},
    io::open_source,
    skv::{CompileOptions, SkvSource, compile},
    source::{SourceSpec, WindowSource},
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

const EDGE: usize = 64;
const BANDS: usize = 6;
const PAGE: usize = 8240;
const RECORD: usize = 128;
const WINDOW: [usize; 4] = [1, 0, 60, 53];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    begin: usize,
    end: usize,
    wire_bytes: usize,
}
impl Request {
    fn body_bytes(&self) -> usize {
        self.end - self.begin
    }
}
struct Server {
    url: String,
    raw_start: usize,
    requests: Arc<Mutex<Vec<Request>>>,
    corrupt: Arc<AtomicUsize>,
    cancel_raw: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    changed: Arc<AtomicBool>,
    denied: Arc<Mutex<Vec<(usize, usize)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>) -> Self {
        let raw_start = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/data.skv", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let corrupt = Arc::new(AtomicUsize::new(usize::MAX));
        let cancel_raw = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
        let stop = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(AtomicBool::new(false));
        let denied = Arc::new(Mutex::new(Vec::<(usize, usize)>::new()));
        let (changed_server, denied_server) = (changed.clone(), denied.clone());
        let (recorded, corrupt_at, stopped) = (requests.clone(), corrupt.clone(), stop.clone());
        let cancel_on_raw = cancel_raw.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
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
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                let text = String::from_utf8(request.clone())
                    .unwrap()
                    .to_ascii_lowercase();
                if changed_server.load(Ordering::Acquire) {
                    let _ = socket.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                if text.starts_with("head ") {
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixed\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
                    continue;
                }
                assert!(text.starts_with("get "));
                // A newly registered known SKV may acquire its generation from
                // the bounded bootstrap GET. Every later range is conditional.
                assert!(
                    text.contains("if-match: \"fixed\"")
                        || text.lines().any(|line| line == "range: bytes=0-16383")
                );
                let value = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (first, last) = value.trim().split_once('-').unwrap();
                let begin = first.parse::<usize>().unwrap();
                let end = last.parse::<usize>().unwrap() + 1;
                assert!(begin < end && end <= bytes.len());
                if denied_server
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|&(a, b)| begin < b && end > a)
                {
                    let _ = socket.write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n",
                    end - begin,
                    begin,
                    end - 1,
                    bytes.len()
                );
                recorded.lock().unwrap().push(Request {
                    begin,
                    end,
                    wire_bytes: request.len() + header.len() + end - begin,
                });
                let mut body = bytes[begin..end].to_vec();
                let corrupt = corrupt_at.load(Ordering::Acquire);
                if (begin..end).contains(&corrupt) {
                    body[corrupt - begin] ^= 0x40;
                }
                let _ = socket.write_all(header.as_bytes());
                if begin >= raw_start {
                    if let Some(cancel) = cancel_on_raw.lock().unwrap().as_ref() {
                        cancel.store(true, Ordering::Release);
                    }
                }
                let _ = socket.write_all(&body);
            }
        });
        Self {
            url,
            raw_start,
            requests,
            corrupt,
            cancel_raw,
            changed,
            denied,
            stop,
            worker: Some(worker),
        }
    }
    fn spec(&self, cap: usize) -> SourceSpec {
        serde_json::from_value(json!({"location":self.url,"http":{
            "allow_http":true,"cache_bytes":0,"max_range_bytes":cap,
            "max_requests":256,"max_download_bytes":16 << 20,"timeout_seconds":3
        }}))
        .unwrap()
    }
    fn raw_requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.begin >= self.raw_start)
            .cloned()
            .collect()
    }
    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    original: PathBuf,
    bands: usize,
    bytes: Vec<u8>,
}
impl Fixture {
    fn new(codec: &str) -> Self {
        Self::with_bands(codec, BANDS, 4)
    }
    fn with_bands(codec: &str, bands: usize, band_group: usize) -> Self {
        Self::with_shape(codec, bands, band_group, EDGE * 2, EDGE)
    }
    fn with_shape(
        codec: &str,
        bands: usize,
        band_group: usize,
        width: usize,
        height: usize,
    ) -> Self {
        Self::with_shape_and_layout(codec, bands, band_group, width, height, "band")
    }
    fn with_shape_and_layout(
        codec: &str,
        bands: usize,
        band_group: usize,
        width: usize,
        height: usize,
        payload_layout: &str,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("six.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<f32, _>(&original, width, height, bands)
            .unwrap();
        ds.set_geo_transform(&[0., 1., 0., EDGE as f64, 0., -1.])
            .unwrap();
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        for index in 0..bands {
            let cells = width * height;
            let mut values = (0..cells)
                .map(|cell| (index * 10_000 + cell) as f32)
                .collect::<Vec<_>>();
            values[5] = f32::from_bits(0x7fc0_0000 + index as u32 + 1);
            values[6] = -0.;
            values[7] = f32::INFINITY;
            values[9] = -9999.;
            let mut band = ds.rasterband(index + 1).unwrap();
            band.write(
                (0, 0),
                (width, height),
                &mut Buffer::new((width, height), values),
            )
            .unwrap();
            band.set_scale(1.25).unwrap();
            band.set_offset(-(index as f64)).unwrap();
            band.set_no_data_value(Some(-9999.)).unwrap();
            band.create_mask_band(false).unwrap();
            let mask = (0..cells)
                .map(|cell| [0u8, 127, 255, 1][(cell + index) % 4])
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
        let spec = serde_json::from_value(json!({"location":original})).unwrap();
        let cancel = AtomicBool::new(false);
        let source = open_source(&spec, &cancel).unwrap();
        let destination = directory.path().join("six.skv");
        compile(
            source.as_ref(),
            destination.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: EDGE,
                band_group,
                payload_layout: payload_layout.into(),
                codec: codec.into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let bytes = fs::read(destination).unwrap();
        Self {
            _directory: directory,
            original,
            bands,
            bytes,
        }
    }
    fn interval(&self, tile: usize, band: usize) -> (usize, usize) {
        let id = tile * self.bands + band;
        let address = 16384 + (id / 64) * PAGE + 16 + (id % 64) * RECORD;
        let begin =
            u64::from_le_bytes(self.bytes[address..address + 8].try_into().unwrap()) as usize;
        let size =
            u32::from_le_bytes(self.bytes[address + 8..address + 12].try_into().unwrap()) as usize;
        (begin, begin + size)
    }
    fn assert_output(&self, source: &dyn WindowSource, bands: &[usize]) {
        self.assert_window_output(source, bands, WINDOW);
    }
    fn assert_window_output(&self, source: &dyn WindowSource, bands: &[usize], window: [usize; 4]) {
        let cancel = AtomicBool::new(false);
        let original = open_source(
            &serde_json::from_value(json!({"location":self.original})).unwrap(),
            &cancel,
        )
        .unwrap();
        let [x, y, w, h] = window;
        let bound = source.raw_read_buffer_bound(w, h, bands).unwrap();
        assert_eq!(bound, w * h * bands.len() * 5 + (8 << 20));
        let (actual, _) = source
            .read_raw_selected_window_cancellable(x, y, w, h, bands, bound, &cancel)
            .unwrap();
        let normalized = actual
            .normalize(
                source.raw_metadata().unwrap(),
                source.metadata(),
                x,
                y,
                bands,
                32 << 20,
                &cancel,
            )
            .unwrap();
        for (number, group) in bands.chunks(20).enumerate() {
            let first = number * 20;
            let mapped = group
                .iter()
                .map(|&band| source.raw_metadata().unwrap().bands[band].original_band_index)
                .collect::<Vec<_>>();
            let (expected, _) = original
                .read_raw_selected_window_cancellable(x, y, w, h, &mapped, 32 << 20, &cancel)
                .unwrap();
            for (got, want) in actual.bands[first..].iter().zip(&expected.bands) {
                assert_eq!(got.samples_le, want.samples_le);
                assert_eq!(got.mask, want.mask);
            }
            let (reference, _) = original
                .read_selected_window_cancellable(x, y, w, h, &mapped, 32 << 20, &cancel)
                .unwrap();
            for (got, want) in normalized.bands[first..].iter().zip(&reference.bands) {
                assert_eq!(got.valid, want.valid);
                assert_eq!(
                    got.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    want.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                );
            }
        }
    }
    fn assert_exact_bytes(&self, requests: &[Request], bands: &[usize]) {
        self.assert_exact_tiles(requests, bands, &[0]);
    }
    fn assert_exact_tiles(&self, requests: &[Request], bands: &[usize], tiles: &[usize]) {
        fn normalized(mut intervals: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
            intervals.sort_unstable();
            let mut result: Vec<(usize, usize)> = Vec::new();
            for (begin, end) in intervals {
                if let Some(previous) = result.last_mut() {
                    assert!(
                        begin >= previous.1,
                        "no duplicate or overlapping payload reads"
                    );
                    if begin == previous.1 {
                        previous.1 = end;
                        continue;
                    }
                }
                result.push((begin, end));
            }
            result
        }
        let actual = normalized(requests.iter().map(|r| (r.begin, r.end)).collect());
        let expected = normalized(
            tiles
                .iter()
                .flat_map(|&tile| bands.iter().map(move |&band| self.interval(tile, band)))
                .collect(),
        );
        assert_eq!(
            actual, expected,
            "raw byte sets must exclude gaps, other bands and other spatial tiles"
        );
    }
}

#[test]
fn dense_permuted_group_reduces_wire_requests_without_extra_payload_bytes() {
    for codec in ["none", "deflate"] {
        let fixture = Fixture::new(codec);
        let server = Server::new(fixture.bytes.clone());
        let cancel = AtomicBool::new(false);
        let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
        for band in 0..4 {
            fixture.assert_output(&source, &[band]);
        }
        let baseline = server.raw_requests();
        assert_eq!(baseline.len(), 4);
        server.clear_requests();
        let selected = [3, 1, 0, 2];
        fixture.assert_output(&source, &selected);
        let grouped = server.raw_requests();
        assert_eq!(
            grouped.len(),
            1,
            "adjacent required payloads should share one GET"
        );
        fixture.assert_exact_bytes(&grouped, &selected);
        assert_eq!(
            grouped.iter().map(Request::body_bytes).sum::<usize>(),
            baseline.iter().map(Request::body_bytes).sum::<usize>()
        );
        assert!(
            grouped.iter().map(|r| r.wire_bytes).sum::<usize>()
                < baseline.iter().map(|r| r.wire_bytes).sum::<usize>()
        );
        // cache_bytes=0 must not turn a second request into a warm-cache claim.
        fixture.assert_output(&source, &selected);
        assert_eq!(server.raw_requests().len(), 2);
    }
}

#[test]
fn sparse_and_separate_physical_groups_never_fetch_unselected_gaps() {
    let fixture = Fixture::new("none");
    let cancel = AtomicBool::new(false);
    for (selection, requests) in [
        (vec![2, 0], 2),
        (vec![4, 0, 3], 3),
        (vec![5, 3, 1, 4, 0, 2], 2),
    ] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
        fixture.assert_output(&source, &selection);
        let observed = server.raw_requests();
        assert_eq!(observed.len(), requests);
        fixture.assert_exact_bytes(&observed, &selection);
    }
}

#[test]
fn configured_range_cap_splits_dense_groups_and_rejects_oversized_individual_payloads() {
    let fixture = Fixture::new("none");
    let cancel = AtomicBool::new(false);
    for (cap, expected_gets) in [(2 * EDGE * EDGE * 5, 2), (EDGE * EDGE * 5, 4)] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(cap), &cancel).unwrap();
        fixture.assert_output(&source, &[3, 1, 0, 2]);
        let observed = server.raw_requests();
        assert_eq!(observed.len(), expected_gets);
        assert!(observed.iter().all(|r| r.body_bytes() <= cap));
        fixture.assert_exact_bytes(&observed, &[0, 1, 2, 3]);
    }
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&server.spec(16_384), &cancel).unwrap();
    let [x, y, w, h] = WINDOW;
    let error = source
        .read_raw_selected_window_cancellable(x, y, w, h, &[0], 16 << 20, &cancel)
        .err()
        .expect("individual payloads must obey the configured HTTP range budget");
    assert!(error.to_string().contains("per-request byte budget"));
    assert!(
        server.raw_requests().is_empty(),
        "oversized raw GET must be rejected before transport"
    );
}

#[test]
fn later_coalesced_record_corruption_rejects_without_partial_result_and_is_sticky() {
    let fixture = Fixture::new("none");
    let server = Server::new(fixture.bytes.clone());
    let cancel = AtomicBool::new(false);
    let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
    server
        .corrupt
        .store(fixture.interval(0, 3).0 + 10, Ordering::Release);
    let [x, y, w, h] = WINDOW;
    let result =
        source.read_selected_window_cancellable(x, y, w, h, &[3, 1, 0, 2], 16 << 20, &cancel);
    let error = result
        .err()
        .expect("a later record checksum must reject the whole window");
    assert!(error.to_string().contains("checksum"), "{error:#}");
    assert_eq!(server.raw_requests().len(), 1);
    assert_eq!(source.diagnostics()["invalidated"], true);
    let before = server.requests.lock().unwrap().len();
    server.corrupt.store(usize::MAX, Ordering::Release);
    assert!(
        source
            .read_selected_window_cancellable(x, y, w, h, &[0], 16 << 20, &cancel)
            .is_err()
    );
    assert_eq!(
        server.requests.lock().unwrap().len(),
        before,
        "quarantined readers must not access transport"
    );
    let reopened = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
    fixture.assert_output(&reopened, &[3, 1, 0, 2]);
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]})
}
fn wide_options(bands: usize) -> Options {
    Options {
        bands: (0..bands).map(|i| (i * 17) % bands).collect(),
        statistics: Some(
            ["sum", "support", "mean", "min", "max"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Default::default()
    }
}
fn wide_single(source: &dyn WindowSource, zone: &Value, options: &Options) -> Value {
    streaming::measure_cached_source(
        source,
        zone,
        "EPSG:3857",
        options,
        &AtomicBool::new(false),
        &mut TileCache::default(),
        0.,
    )
    .unwrap()
}
fn wide_batch(
    source: &dyn WindowSource,
    zones: &[Value],
    options: &Options,
    tile_bytes: usize,
) -> Value {
    let spec: JobSpec = serde_json::from_value(json!({
        "zones":zones.iter().enumerate().map(|(i,g)|json!({"id":i.to_string(),"version":"1","geometry":g})).collect::<Vec<_>>(),
        "slices":[{"id":"one","source":"r"}],"crs":"EPSG:3857","tile_edge":64,
        "options":options,"budget":{"tile_bytes":tile_bytes}
    })).unwrap();
    Job::new(spec, None, 4096, 1 << 30)
        .unwrap()
        .next(
            zones.len(),
            |_| Ok(Box::new(BorrowedSource(source))),
            &AtomicBool::new(false),
        )
        .unwrap()
}

#[test]
fn forty_band_source_capability_coalesces_one_raw_range_and_preserves_permuted_bits() {
    for codec in ["none", "deflate"] {
        let fixture = Fixture::with_bands(codec, 40, 40);
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
        assert_eq!(source.max_read_bands(), 64);
        let bands = wide_options(40).bands;
        fixture.assert_output(&source, &bands);
        let requests = server.raw_requests();
        assert_eq!(
            requests.len(),
            1,
            "all40 selected chunks belong to one bounded physical range"
        );
        fixture.assert_exact_bytes(&requests, &bands);
        assert_eq!(source.diagnostics()["metrics"]["coalesced_raw_chunks"], 40);
        assert_eq!(source.diagnostics()["remote"]["cache_capacity_bytes"], 0);
    }
}

#[test]
fn wide_single_and_shared_batch_preserve_thin_answers_and_never_fetch_full_interior() {
    let fixture = Fixture::with_bands("deflate", 40, 40);
    let cancel = AtomicBool::new(false);
    let original = open_source(
        &serde_json::from_value(json!({"location":fixture.original})).unwrap(),
        &cancel,
    )
    .unwrap();
    assert_eq!(original.max_read_bands(), 20);
    let options = wide_options(40);
    let zones = [
        rectangle(-1., -1., 95.5, 65.),
        rectangle(70.25, 2., 70.250000001, 60.),
    ];
    let expected = zones
        .iter()
        .map(|z| wide_single(original.as_ref(), z, &options))
        .collect::<Vec<_>>();
    for summaries in [true, false] {
        let server = Server::new(fixture.bytes.clone());
        let mut spec = server.spec(4 << 20);
        spec.use_summaries = summaries;
        let source = SkvSource::open(&spec, &cancel).unwrap();
        for (zone, reference) in zones.iter().zip(&expected) {
            server.clear_requests();
            let actual = wide_single(&source, zone, &options);
            assert_eq!(actual["bands"], reference["bands"]);
            if summaries {
                assert_eq!(server.raw_requests().len(), 1, "{actual}");
                let interior_end = fixture.interval(0, 39).1;
                assert!(
                    server
                        .raw_requests()
                        .iter()
                        .all(|r| r.begin >= interior_end)
                );
            }
        }
        assert!(
            expected[1]["bands"][0]["covered_cell_equivalents"]
                .as_f64()
                .unwrap()
                > 0.
        );
        server.clear_requests();
        let actual = wide_batch(&source, &zones, &options, 64 << 20);
        let control = wide_batch(original.as_ref(), &zones, &options, 64 << 20);
        assert_eq!(actual["complete"], true);
        for i in 0..zones.len() {
            assert_eq!(actual["rows"][i]["bands"], control["rows"][i]["bands"]);
        }
        if summaries {
            assert_eq!(actual["metrics"]["band_group_reads"], 1);
            assert_eq!(server.raw_requests().len(), 1);
            assert!(server.raw_requests()[0].begin >= fixture.interval(0, 39).1);
        }
    }
}

#[test]
fn wide_batch_shrinks_to_existing_byte_budget_and_64_is_a_source_only_cap() {
    for count in [40, 64] {
        let fixture = Fixture::with_bands("none", count, count);
        let server = Server::new(fixture.bytes.clone());
        let cancel = AtomicBool::new(false);
        let mut spec = server.spec(4 << 20);
        spec.use_summaries = false;
        let source = SkvSource::open(&spec, &cancel).unwrap();
        let mut options = wide_options(count);
        options.bands = (0..count).collect();
        let zones = [rectangle(0.25, 0.25, 63.75, 63.75)];
        let unbounded = wide_batch(&source, &zones, &options, 64 << 20);
        assert_eq!(unbounded["metrics"]["band_group_reads"], 1);
        server.clear_requests();
        let limited = wide_batch(&source, &zones, &options, 10 << 20);
        assert_eq!(limited["rows"][0]["bands"], unbounded["rows"][0]["bands"]);
        assert_eq!(limited["metrics"]["band_group_reads"], 2);
        assert_eq!(
            server.raw_requests().len(),
            2,
            "smaller admission partitions selected chunks without hidden overread"
        );
        fixture.assert_exact_bytes(&server.raw_requests(), &options.bands);
        let (raster, _) = source
            .read_selected_window_cancellable(0, 0, EDGE, EDGE, &options.bands, 64 << 20, &cancel)
            .unwrap();
        assert_eq!(raster.bands.len(), count);
        assert!(
            raster.validate().is_err(),
            "the public resident Raster contract remains20"
        );
        let cancelled = AtomicBool::new(true);
        server.clear_requests();
        assert!(
            source
                .read_raw_selected_window_cancellable(
                    0,
                    0,
                    EDGE,
                    EDGE,
                    &options.bands,
                    64 << 20,
                    &cancelled
                )
                .is_err()
        );
        assert!(server.raw_requests().is_empty());
        assert_eq!(source.diagnostics()["invalidated"], true);
        let source = SkvSource::open(&spec, &cancel).unwrap();
        assert!(
            source
                .read_raw_selected_window_cancellable(
                    0,
                    0,
                    EDGE,
                    EDGE,
                    &vec![0; 65],
                    64 << 20,
                    &cancel
                )
                .is_err()
        );
        let source = SkvSource::open(&spec, &cancel).unwrap();
        let bound = source
            .read_buffer_bound(EDGE, EDGE, &options.bands)
            .unwrap();
        server.clear_requests();
        assert!(
            source
                .read_selected_window_cancellable(
                    0,
                    0,
                    EDGE,
                    EDGE,
                    &options.bands,
                    bound - 1,
                    &cancel
                )
                .is_err()
        );
        assert!(
            server.raw_requests().is_empty(),
            "budget rejection occurs before raw read"
        );
    }
}

#[test]
fn adjacent_two_and_three_tile_reads_coalesce_with_identical_bits_and_payload_bytes() {
    for codec in ["none", "deflate"] {
        for tiles in [2, 3] {
            let width = EDGE * tiles - 9;
            let height = EDGE - 11;
            let fixture = Fixture::with_shape(codec, 40, 40, width, height);
            let server = Server::new(fixture.bytes.clone());
            let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
            let bands = wide_options(40).bands;
            for tile in 0..tiles {
                fixture.assert_window_output(
                    &source,
                    &bands,
                    [tile * EDGE, 0, EDGE.min(width - tile * EDGE), height],
                );
            }
            let baseline = server.raw_requests();
            assert_eq!(baseline.len(), tiles);
            server.clear_requests();
            fixture.assert_window_output(&source, &bands, [3, 2, width - 7, height - 4]);
            let combined = server.raw_requests();
            assert_eq!(
                combined.len(),
                1,
                "adjacent required tile payloads fit one GET"
            );
            fixture.assert_exact_tiles(&combined, &bands, &(0..tiles).collect::<Vec<_>>());
            assert_eq!(
                combined.iter().map(Request::body_bytes).sum::<usize>(),
                baseline.iter().map(Request::body_bytes).sum::<usize>()
            );
            assert!(
                combined.iter().map(|r| r.wire_bytes).sum::<usize>()
                    < baseline.iter().map(|r| r.wire_bytes).sum::<usize>()
            );
            assert_eq!(source.diagnostics()["remote"]["cache_capacity_bytes"], 0);
        }
    }
}

#[test]
fn cross_tile_queue_never_fetches_unselected_tile_or_band_gaps_and_keeps_old_groups() {
    let fixture = Fixture::with_shape("none", 40, 40, EDGE * 3, EDGE * 2);
    let bands = wide_options(40).bands;
    let server = Server::new(fixture.bytes.clone());
    let cancel = AtomicBool::new(false);
    let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
    fixture.assert_window_output(&source, &bands, [1, 1, EDGE - 3, EDGE * 2 - 2]);
    let requests = server.raw_requests();
    assert_eq!(
        requests.len(),
        2,
        "unselected spatial tiles separate these required ranges"
    );
    fixture.assert_exact_tiles(&requests, &bands, &[0, 3]);

    let server = Server::new(fixture.bytes.clone());
    let mut spec = server.spec(4 << 20);
    spec.bands = Some(vec![39, 0, 17]);
    let source = SkvSource::open(&spec, &cancel).unwrap();
    fixture.assert_window_output(&source, &[2, 0, 1], [0, 0, EDGE * 3, EDGE]);
    let requests = server.raw_requests();
    fixture.assert_exact_tiles(&requests, &[17, 39, 0], &[0, 1, 2]);
    assert_eq!(
        requests.len(),
        7,
        "only each tile's last/next first selected band may join"
    );

    let grouped = Fixture::with_shape("none", 6, 4, EDGE * 3, EDGE);
    let server = Server::new(grouped.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
    grouped.assert_window_output(&source, &[5, 3, 1, 4, 0, 2], [0, 0, EDGE * 3, EDGE]);
    assert_eq!(
        server.raw_requests().len(),
        6,
        "backward offsets and group gaps retain separate ranges"
    );
    grouped.assert_exact_tiles(&server.raw_requests(), &[0, 1, 2, 3, 4, 5], &[0, 1, 2]);
}

#[test]
fn cross_tile_coalescing_respects_range_and_record_queue_caps() {
    let fixture = Fixture::with_shape("none", 40, 40, EDGE * 3, EDGE);
    let chunk_bytes = EDGE * EDGE * 5;
    for chunks_per_range in [1, 45] {
        let cap = chunks_per_range * chunk_bytes;
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(cap), &AtomicBool::new(false)).unwrap();
        let bands = wide_options(40).bands;
        fixture.assert_window_output(&source, &bands, [0, 0, EDGE * 3, EDGE]);
        let requests = server.raw_requests();
        assert_eq!(requests.len(), 120usize.div_ceil(chunks_per_range));
        assert!(requests.iter().all(|r| r.body_bytes() <= cap));
        fixture.assert_exact_tiles(&requests, &bands, &[0, 1, 2]);
    }
    let fixture = Fixture::with_shape("deflate", 40, 40, EDGE * 7, EDGE);
    let begin = fixture.interval(0, 0).0;
    let end = fixture.interval(6, 39).1;
    assert!(
        end - begin < 4 << 20,
        "queue-limit fixture must fit the independent byte cap"
    );
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    let bands = wide_options(40).bands;
    fixture.assert_window_output(&source, &bands, [0, 0, EDGE * 7, EDGE]);
    let requests = server.raw_requests();
    assert_eq!(
        requests.len(),
        2,
        "280 required records exceed the256-entry queue"
    );
    assert_eq!(requests[0].end, fixture.interval(6, 15).1);
    fixture.assert_exact_tiles(&requests, &bands, &(0..7).collect::<Vec<_>>());
}

#[test]
fn corruption_and_cancellation_in_cross_tile_range_fail_closed_and_stay_invalidated() {
    let fixture = Fixture::with_shape("none", 40, 40, EDGE * 3, EDGE);
    let bands = wide_options(40).bands;
    for corrupt in [true, false] {
        let server = Server::new(fixture.bytes.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
        if corrupt {
            server
                .corrupt
                .store(fixture.interval(2, 39).0 + 5, Ordering::Release);
        } else {
            *server.cancel_raw.lock().unwrap() = Some(cancel.clone());
        }
        let error = source
            .read_raw_selected_window_cancellable(0, 0, EDGE * 3, EDGE, &bands, 32 << 20, &cancel)
            .err()
            .expect("whole cross-tile operation must reject");
        if corrupt {
            assert!(error.to_string().contains("checksum"), "{error:#}");
        }
        assert_eq!(server.raw_requests().len(), 1);
        assert_eq!(source.diagnostics()["invalidated"], true);
        let before = server.requests.lock().unwrap().len();
        cancel.store(false, Ordering::Release);
        server.corrupt.store(usize::MAX, Ordering::Release);
        *server.cancel_raw.lock().unwrap() = None;
        assert!(
            source
                .read_raw_selected_window_cancellable(0, 0, EDGE, EDGE, &[0], 32 << 20, &cancel)
                .is_err()
        );
        assert_eq!(
            server.requests.lock().unwrap().len(),
            before,
            "a sticky failure must not resume transport"
        );
    }
}

fn prefetch_spec(server: &Server, enabled: bool) -> SourceSpec {
    let mut spec = server.spec(4 << 20);
    spec.http.cache_bytes = 4 << 20;
    spec.http.max_requests = 2048;
    spec.http.boundary_read_ahead = enabled;
    spec
}
fn interval_union(mut input: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    input.sort_unstable();
    input.dedup();
    let mut result: Vec<(usize, usize)> = Vec::new();
    for (a, b) in input {
        if let Some(last) = result.last_mut() {
            assert!(a >= last.1, "unexpected partial overlap");
            if a == last.1 {
                last.1 = b;
                continue;
            }
        }
        result.push((a, b));
    }
    result
}
#[test]
fn summary_boundary_pair_prefetch_reduces_gets_without_changing_values_or_byte_union() {
    for (layout, band_group) in [("band", 1), ("row_group_v1", 40)] {
        let fixture =
            Fixture::with_shape_and_layout("deflate", 40, band_group, EDGE * 2, EDGE, layout);
        let server = Server::new(fixture.bytes.clone());
        let polygon = rectangle(1., 1., (EDGE * 2 - 1) as f64, (EDGE - 1) as f64);
        for selection in [
            vec![7],
            (0..40).collect(),
            std::iter::once(39).chain(0..35).collect(),
        ] {
            let mut options = wide_options(40);
            options.bands = selection;
            let before =
                SkvSource::open(&prefetch_spec(&server, false), &AtomicBool::new(false)).unwrap();
            server.clear_requests();
            let reference = wide_single(&before, &polygon, &options);
            let original_requests = server.raw_requests();
            let after =
                SkvSource::open(&prefetch_spec(&server, true), &AtomicBool::new(false)).unwrap();
            server.clear_requests();
            let result = wide_single(&after, &polygon, &options);
            let optimized_requests = server.raw_requests();
            assert_eq!(
                serde_json::to_vec(&result["bands"]).unwrap(),
                serde_json::to_vec(&reference["bands"]).unwrap()
            );
            assert!(
                optimized_requests.len() < original_requests.len(),
                "{layout}, {:?}: {:?} >= {:?}",
                options.bands,
                optimized_requests,
                original_requests
            );
            assert_eq!(
                interval_union(
                    optimized_requests
                        .iter()
                        .map(|r| (r.begin, r.end))
                        .collect()
                ),
                interval_union(original_requests.iter().map(|r| (r.begin, r.end)).collect())
            );
            assert_eq!(
                optimized_requests
                    .iter()
                    .map(Request::body_bytes)
                    .sum::<usize>(),
                original_requests
                    .iter()
                    .map(Request::body_bytes)
                    .sum::<usize>()
            );
            assert_eq!(
                result["work"]["raw_positive_cells"],
                reference["work"]["raw_positive_cells"]
            );
            assert_eq!(
                after.diagnostics()["metrics"]["decoder_calls"],
                before.diagnostics()["metrics"]["decoder_calls"]
            );
            assert!(
                after.diagnostics()["metrics"]["boundary_prefetch_admissions"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(
                after.diagnostics()["metrics"]["native_raw_intermediate_allocated_bytes"],
                0
            );
        }
    }
}
#[test]
fn summary_prefetch_respects_protected_interiors_and_retained_decode_hits() {
    let fixture = Fixture::with_shape_and_layout("deflate", 6, 1, EDGE * 3, EDGE * 3, "band");
    let server = Server::new(fixture.bytes.clone());
    *server.denied.lock().unwrap() = (0..6).map(|band| fixture.interval(4, band)).collect();
    let polygon = rectangle(
        1.,
        1. - (EDGE * 2) as f64,
        (EDGE * 3 - 1) as f64,
        (EDGE - 1) as f64,
    );
    let options = wide_options(6);
    let reference_source =
        SkvSource::open(&prefetch_spec(&server, false), &AtomicBool::new(false)).unwrap();
    let reference = wide_single(&reference_source, &polygon, &options);
    let source = SkvSource::open(&prefetch_spec(&server, true), &AtomicBool::new(false)).unwrap();
    let mut cache = TileCache::default();
    cache.set_limit(64 << 20).unwrap();
    for iteration in 0..2 {
        server.clear_requests();
        let value = streaming::measure_cached_source(
            &source,
            &polygon,
            "EPSG:3857",
            &options,
            &AtomicBool::new(false),
            &mut cache,
            0.,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&value["bands"]).unwrap(),
            serde_json::to_vec(&reference["bands"]).unwrap()
        );
        assert!(
            value["work"]["eligible_raw_interior_tiles_avoided"]
                .as_u64()
                .unwrap()
                > 0
        );
        if iteration == 0 {
            assert!(
                source.diagnostics()["metrics"]["boundary_prefetch_admissions"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
        if iteration == 1 {
            assert!(
                server.raw_requests().is_empty(),
                "decoded cache hits must not prefetch raw bytes"
            );
        }
    }
}
#[test]
fn prefetched_later_corruption_cancellation_and_changed_generation_fail_closed() {
    let fixture = Fixture::with_shape_and_layout("none", 6, 1, EDGE * 2, EDGE, "band");
    let windows = [[0, 0, EDGE, EDGE], [EDGE, 0, EDGE, EDGE]];
    let bands = [0, 1, 2, 3, 4, 5];
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&prefetch_spec(&server, true), &AtomicBool::new(false)).unwrap();
    server
        .corrupt
        .store(fixture.interval(1, 5).0 + 10, Ordering::Release);
    source
        .prefetch_boundary_windows(&windows, &bands, 64 << 20, &AtomicBool::new(false))
        .unwrap();
    assert!(
        source.diagnostics()["metrics"]["boundary_prefetch_admissions"]
            .as_u64()
            .unwrap()
            > 0
    );
    let prepared_raw_requests = server.raw_requests().len();
    assert!(
        source
            .read_selected_window_cancellable(
                EDGE,
                0,
                EDGE,
                EDGE,
                &bands,
                64 << 20,
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(
        server.raw_requests().len(),
        prepared_raw_requests,
        "corrupt prefetched bytes must fail without another raw GET"
    );
    assert_eq!(source.diagnostics()["invalidated"], true);
    let count = server.requests.lock().unwrap().len();
    assert!(
        source
            .prefetch_boundary_windows(&windows, &bands, 64 << 20, &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(server.requests.lock().unwrap().len(), count);

    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&prefetch_spec(&server, true), &AtomicBool::new(false)).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    *server.cancel_raw.lock().unwrap() = Some(cancel.clone());
    assert!(
        source
            .prefetch_boundary_windows(&windows, &bands, 64 << 20, &cancel)
            .is_err()
    );
    assert_eq!(source.diagnostics()["invalidated"], true);

    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&prefetch_spec(&server, true), &AtomicBool::new(false)).unwrap();
    source
        .prefetch_boundary_windows(&windows, &bands, 64 << 20, &AtomicBool::new(false))
        .unwrap();
    server.changed.store(true, Ordering::Release);
    assert!(source.verify_immutable().is_err());
    assert_eq!(source.diagnostics()["invalidated"], true);
    assert!(
        source
            .read_selected_window_cancellable(
                0,
                0,
                EDGE,
                EDGE,
                &bands,
                64 << 20,
                &AtomicBool::new(false)
            )
            .is_err()
    );
}
