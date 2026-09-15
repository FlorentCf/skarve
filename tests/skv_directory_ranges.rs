//! Remote directory read-ahead is bounded metadata overread, never raw prefetch.
use anyhow::{Result, ensure};
use raster_engine::{
    aggregate::Options,
    model::{Grid, Raster, check_cancel},
    skv::{self, BOOTSTRAP, CompileOptions, PAGE, SkvSource},
    source::{
        BandMetadata, RasterMetadata, RawBandMetadata, RawBandWindow, RawRasterMetadata,
        RawScalarType, RawWindow, ReadMetrics, SourceSpec, WindowSource,
    },
    stored_summary::StoredSummarySource,
    streaming,
    tile_cache::TileCache,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct ConstantTiles {
    metadata: RasterMetadata,
    raw: RawRasterMetadata,
}
impl ConstantTiles {
    fn new(leaves: usize) -> Self {
        let width = leaves * 64;
        Self {
            metadata: RasterMetadata {
                grid: Grid {
                    width,
                    height: 1,
                    transform: [0., 1., 0., 1., 0., -1.],
                    crs: "LOCAL".into(),
                },
                bands: vec![BandMetadata {
                    data_type: "Byte".into(),
                    nodata: None,
                    scale: 1.,
                    offset: 0.,
                    unit: None,
                    block_size: (64, 1),
                }],
                source_id: "bounded-directory-fixture".into(),
            },
            raw: RawRasterMetadata {
                bands: vec![RawBandMetadata {
                    scalar_type: RawScalarType::Byte,
                    nodata_f64_bits: None,
                    scale_f64_bits: 1f64.to_bits(),
                    offset_f64_bits: 0f64.to_bits(),
                    unit: None,
                    mask_flags: 1,
                    original_band_index: 0,
                    description: "constant per tile".into(),
                }],
                pixel_convention: "Area".into(),
                source_band_count: 1,
                source_overview: None,
            },
        }
    }
}
impl WindowSource for ConstantTiles {
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn raw_metadata(&self) -> Option<&RawRasterMetadata> {
        Some(&self.raw)
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn raw_read_buffer_bound(&self, w: usize, h: usize, bands: &[usize]) -> Result<usize> {
        ensure!(bands == [0], "fixture bands");
        Ok(w * h * 2)
    }
    fn read_raw_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max: usize,
        cancel: &AtomicBool,
    ) -> Result<(RawWindow, ReadMetrics)> {
        check_cancel(cancel)?;
        ensure!(
            y == 0
                && h == 1
                && x + w <= self.metadata.grid.width
                && self.raw_read_buffer_bound(w, h, bands)? <= max,
            "fixture window"
        );
        Ok((
            RawWindow {
                width: w,
                height: h,
                bands: vec![RawBandWindow {
                    samples_le: (x..x + w).map(|v| ((v / 64) % 200 + 1) as u8).collect(),
                    mask: vec![255; w],
                }],
            },
            ReadMetrics {
                raster_io_calls: 1,
                ..Default::default()
            },
        ))
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        max: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        let (raw, metrics) =
            self.read_raw_selected_window_cancellable(x, y, w, h, bands, max, cancel)?;
        Ok((
            raw.normalize(&self.raw, &self.metadata, x, y, bands, max, cancel)?,
            metrics,
        ))
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    pages: usize,
}
impl Fixture {
    fn new(leaves: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("directory.skv");
        skv::compile(
            &ConstantTiles::new(leaves),
            path.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: 64,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let pages = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
        Self {
            _dir: dir,
            path,
            bytes,
            pages,
        }
    }
    fn local(&self) -> SkvSource {
        SkvSource::open(
            &serde_json::from_value(json!({"location":self.path,"http":{"cache_bytes":0}}))
                .unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    begin: usize,
    end: usize,
}
struct Server {
    url: String,
    data_offset: usize,
    requests: Arc<Mutex<Vec<Request>>>,
    corrupt: Arc<AtomicUsize>,
    changed: Arc<AtomicBool>,
    hold: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>) -> Self {
        let data_offset = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/source.skv", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let corrupt = Arc::new(AtomicUsize::new(usize::MAX));
        let changed = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, bad, etag, held, seen, gate, done) = (
            requests.clone(),
            corrupt.clone(),
            changed.clone(),
            hold.clone(),
            observed.clone(),
            release.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
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
                let text = String::from_utf8(request).unwrap().to_ascii_lowercase();
                if text.starts_with("head ") {
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixed\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
                    continue;
                }
                assert!(text.starts_with("get "));
                // A newly registered known SKV may acquire its generation from
                // the bounded bootstrap GET. Every later range is conditional.
                assert!(
                    text.contains("if-match: \"fixed\"")
                        || text.lines().any(|line| line == "range: bytes=0-16383")
                );
                let range = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (first, last) = range.trim().split_once('-').unwrap();
                let (begin, end) = (
                    first.parse::<usize>().unwrap(),
                    last.parse::<usize>().unwrap() + 1,
                );
                assert!(begin < end && end <= bytes.len());
                recorded.lock().unwrap().push(Request { begin, end });
                let directory = (BOOTSTRAP..data_offset).contains(&begin);
                let tag = if directory && etag.load(Ordering::Acquire) {
                    "changed"
                } else {
                    "fixed"
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"{}\"\r\nConnection: close\r\n\r\n",
                    end - begin,
                    begin,
                    end - 1,
                    bytes.len(),
                    tag
                );
                if directory && held.load(Ordering::Acquire) {
                    seen.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !gate.load(Ordering::Acquire)
                        && !done.load(Ordering::Acquire)
                        && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let mut body = bytes[begin..end].to_vec();
                let corrupt = bad.load(Ordering::Acquire);
                if (begin..end).contains(&corrupt) {
                    body[corrupt - begin] ^= 0x40;
                }
                let _ = socket.write_all(&body);
            }
        });
        Self {
            url,
            data_offset,
            requests,
            corrupt,
            changed,
            hold,
            observed,
            release,
            stop,
            worker: Some(worker),
        }
    }
    fn spec(&self, cap: usize) -> SourceSpec {
        serde_json::from_value(json!({"location":self.url,"http":{"allow_http":true,"cache_bytes":0,"max_range_bytes":cap,"max_requests":512,"max_download_bytes":16<<20,"timeout_seconds":3}})).unwrap()
    }
    fn directory_requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.begin >= BOOTSTRAP && r.begin < self.data_offset)
            .cloned()
            .collect()
    }
    fn assert_no_raw(&self) {
        assert!(
            self.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.end <= self.data_offset),
            "directory slab touched raw bytes"
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn read_page(source: &SkvSource, page: usize) -> Vec<[u64; 5]> {
    source
        .read_summary(page * 64, &[0], &AtomicBool::new(false))
        .unwrap()
        .into_iter()
        .map(|s| {
            let [a, b] = s.sum.parts();
            [
                a.to_bits(),
                b.to_bits(),
                s.valid_count as u64,
                s.min.to_bits(),
                s.max.to_bits(),
            ]
        })
        .collect()
}
fn metrics(source: &SkvSource) -> Value {
    source.diagnostics()["metrics"].clone()
}

#[test]
fn dense_remote_directory_reads_reduce_gets_preserve_states_and_keep_lru_bounded() {
    let fixture = Fixture::new(2500);
    assert!(fixture.pages > 72);
    let local = fixture.local();
    let server = Server::new(fixture.bytes.clone());
    let remote = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    for page in 0..fixture.pages {
        assert_eq!(read_page(&remote, page), read_page(&local, page));
    }
    let requests = server.directory_requests();
    assert_eq!(requests.len(), fixture.pages.div_ceil(8));
    assert!(requests.len() < fixture.pages);
    assert_eq!(
        requests.iter().map(|r| r.end - r.begin).sum::<usize>(),
        fixture.pages * PAGE
    );
    for (slab, r) in requests.iter().enumerate() {
        assert_eq!(r.begin, BOOTSTRAP + slab * 8 * PAGE);
        assert_eq!(
            r.end,
            BOOTSTRAP + ((slab + 1) * 8).min(fixture.pages) * PAGE
        );
    }
    let m = metrics(&remote);
    assert_eq!(m["directory_bytes"], fixture.pages * PAGE);
    assert_eq!(
        m["directory_pages_prefetched"],
        fixture.pages - fixture.pages.div_ceil(8)
    );
    assert_eq!(m["page_cache_peak_bytes"], 64 * PAGE);
    assert_eq!(m["page_cache_evictions"], fixture.pages - 64);
    assert_eq!(m["raw_encoded_bytes"], 0);
    let all = server.requests.lock().unwrap().clone();
    let transport = remote.diagnostics()["remote"].clone();
    assert_eq!(transport["get_requests"], all.len());
    assert_eq!(
        transport["accepted_bytes"],
        all.iter().map(|r| r.end - r.begin).sum::<usize>()
    );
    assert_eq!(metrics(&local)["directory_bytes"], fixture.pages * PAGE);
    assert_eq!(metrics(&local)["directory_pages_prefetched"], 0);
    assert_eq!(
        remote.retained_memory_bound(),
        local.retained_memory_bound()
    );
    assert_eq!(
        remote.summary_read_buffer_bound(&[0]).unwrap()
            - local.summary_read_buffer_bound(&[0]).unwrap(),
        7 * PAGE
    );
    assert_eq!(read_page(&remote, 0), read_page(&local, 0));
    assert_eq!(server.directory_requests().len(), requests.len() + 1);
    assert_eq!(metrics(&remote)["page_cache_peak_bytes"], 64 * PAGE);
    assert_eq!(
        metrics(&remote)["page_cache_evictions"],
        fixture.pages - 64 + 8
    );
    // A complete ordinary native query stays summary-only and has the same
    // consumed numerical fields as the local reader after the LRU churn.
    let width = remote.metadata().grid.width as f64;
    let geometry = json!({"type":"Polygon","coordinates":[[[-1.,-1.],[width+1.,-1.],[width+1.,2.],[-1.,2.],[-1.,-1.]]]});
    let options = Options {
        statistics: Some(
            ["sum", "support", "mean", "min", "max", "count"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Default::default()
    };
    let run = |source: &SkvSource| {
        streaming::measure_cached_source(
            source,
            &geometry,
            "LOCAL",
            &options,
            &AtomicBool::new(false),
            &mut TileCache::default(),
            0.,
        )
        .unwrap()
    };
    assert_eq!(run(&remote)["bands"], run(&local)["bands"]);
    server.assert_no_raw();
}

#[test]
fn sparse_tiny_final_slabs_and_range_caps_account_for_actual_metadata_overread() {
    let fixture = Fixture::new(300);
    assert_eq!(fixture.pages, 10);
    for cap in [BOOTSTRAP, 2 * PAGE, 3 * PAGE, 8 * PAGE] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(cap), &AtomicBool::new(false)).unwrap();
        let capacity = (cap / PAGE).min(8);
        let page = 1;
        let first = page / capacity * capacity;
        let count = capacity.min(fixture.pages - first);
        read_page(&source, page);
        assert_eq!(
            server.directory_requests(),
            vec![Request {
                begin: BOOTSTRAP + first * PAGE,
                end: BOOTSTRAP + (first + count) * PAGE
            }]
        );
        assert_eq!(metrics(&source)["directory_bytes"], count * PAGE);
        assert_eq!(metrics(&source)["directory_pages_prefetched"], count - 1);
        // Only one record was requested. The excess is disclosed metadata
        // overread, even if a subsequent access can reuse it without a GET.
        assert_eq!(metrics(&source)["summary_states_read"], 1);
        read_page(&source, first);
        assert_eq!(server.directory_requests().len(), 1);
        let last = fixture.pages - 1;
        let last_first = last / capacity * capacity;
        let last_count = capacity.min(fixture.pages - last_first);
        read_page(&source, last);
        assert_eq!(
            server.directory_requests().last().unwrap(),
            &Request {
                begin: BOOTSTRAP + last_first * PAGE,
                end: server.data_offset
            }
        );
        assert_eq!(
            server.data_offset - (BOOTSTRAP + last_first * PAGE),
            last_count * PAGE
        );
        server.assert_no_raw();
    }
    let tiny = Fixture::new(1);
    assert_eq!(tiny.pages, 1);
    let server = Server::new(tiny.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    read_page(&source, 0);
    assert_eq!(
        server.directory_requests(),
        vec![Request {
            begin: BOOTSTRAP,
            end: BOOTSTRAP + PAGE
        }]
    );
    assert_eq!(metrics(&source)["directory_pages_prefetched"], 0);
    server.assert_no_raw();
    assert!(SkvSource::open(&server.spec(PAGE - 1), &AtomicBool::new(false)).is_err());
}

#[test]
fn malformed_prefetched_neighbor_and_changed_generation_publish_no_pages_and_stick() {
    let fixture = Fixture::new(300);
    for changed in [false, true] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
        if changed {
            server.changed.store(true, Ordering::Release);
        } else {
            server
                .corrupt
                .store(BOOTSTRAP + 2 * PAGE - 1, Ordering::Release);
        }
        let error = source
            .read_summary(0, &[0], &AtomicBool::new(false))
            .err()
            .expect("neighbor or generation must be rejected");
        assert!(
            error.to_string().contains(if changed {
                "ETag"
            } else {
                "directory checksum"
            }),
            "{error}"
        );
        assert_eq!(metrics(&source)["page_cache_peak_bytes"], 0);
        assert_eq!(metrics(&source)["directory_pages_prefetched"], 0);
        assert_eq!(metrics(&source)["summary_states_read"], 0);
        let count = server.requests.lock().unwrap().len();
        server.corrupt.store(usize::MAX, Ordering::Release);
        server.changed.store(false, Ordering::Release);
        assert!(
            source
                .read_summary(0, &[0], &AtomicBool::new(false))
                .err()
                .unwrap()
                .to_string()
                .contains("invalidated")
        );
        assert_eq!(server.requests.lock().unwrap().len(), count);
        server.assert_no_raw();
        let reopened = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
        read_page(&reopened, 0);
    }
}

#[test]
fn prefetched_ranges_respect_request_byte_and_cancellation_limits() {
    let fixture = Fixture::new(300);
    for bytes in [false, true] {
        let server = Server::new(fixture.bytes.clone());
        let mut spec = server.spec(4 << 20);
        if bytes {
            spec.http.max_download_bytes = (BOOTSTRAP + PAGE) as u64;
        } else {
            // Known SKV registration now consumes one GET. Leave no request budget.
            spec.http.max_requests = 1;
        }
        let source = SkvSource::open(&spec, &AtomicBool::new(false)).unwrap();
        let count = server.requests.lock().unwrap().len();
        let error = source
            .read_summary(0, &[0], &AtomicBool::new(false))
            .err()
            .expect("slab exceeds request or byte budget");
        assert!(error.to_string().contains("budget"), "{error}");
        assert_eq!(server.requests.lock().unwrap().len(), count);
        assert_eq!(metrics(&source)["page_cache_peak_bytes"], 0);
        server.assert_no_raw();
    }
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    server.hold.store(true, Ordering::Release);
    let cancel = Arc::new(AtomicBool::new(false));
    let (flag, seen, release) = (
        cancel.clone(),
        server.observed.clone(),
        server.release.clone(),
    );
    let interrupter = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !seen.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            seen.load(Ordering::Acquire),
            "directory response was never observed"
        );
        flag.store(true, Ordering::Release);
        release.store(true, Ordering::Release);
    });
    assert!(source.read_summary(0, &[0], &cancel).is_err());
    interrupter.join().unwrap();
    assert_eq!(metrics(&source)["page_cache_peak_bytes"], 0);
    assert_eq!(metrics(&source)["summary_states_read"], 0);
    assert!(
        source
            .read_summary(0, &[0], &AtomicBool::new(false))
            .err()
            .unwrap()
            .to_string()
            .contains("invalidated")
    );
    server.assert_no_raw();
}
