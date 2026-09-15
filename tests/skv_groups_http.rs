//! Independent transport and byte-oracle checks for optional row-group packets.
//! Fixtures are generated data, not a claim about real demographic distributions.
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    aggregate::Options,
    batch::{BorrowedSource, Job, JobSpec},
    io::open_source,
    skv::{CompileOptions, SkvSource, compile},
    source::{RawWindow, SourceSpec, WindowSource},
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

const BOOT: usize = 16384;
const PAGE: usize = 8240;
const RECORD: usize = 128;
const MAX_GROUP: usize = 3_276_800;

#[derive(Clone, Debug)]
struct Range {
    begin: usize,
    end: usize,
}
impl Range {
    fn len(&self) -> usize {
        self.end - self.begin
    }
}

// The transport deliberately retains no response cache. Every GET is recorded.
struct Server {
    url: String,
    raw_start: usize,
    requests: Arc<Mutex<Vec<Range>>>,
    corrupt: Arc<AtomicUsize>,
    cancel_raw: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>) -> Self {
        let raw_start = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/group.skv", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let corrupt = Arc::new(AtomicUsize::new(usize::MAX));
        let cancel_raw = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, corrupt_at, cancel_at, stopped) = (
            requests.clone(),
            corrupt.clone(),
            cancel_raw.clone(),
            stop.clone(),
        );
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
                let text = String::from_utf8(request).unwrap().to_ascii_lowercase();
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
                let range = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (first, last) = range.trim().split_once('-').unwrap();
                let begin = first.parse::<usize>().unwrap();
                let end = last.parse::<usize>().unwrap() + 1;
                assert!(begin < end && end <= bytes.len());
                observed.lock().unwrap().push(Range { begin, end });
                let mut body = bytes[begin..end].to_vec();
                let bad = corrupt_at.load(Ordering::Acquire);
                if (begin..end).contains(&bad) {
                    body[bad - begin] ^= 0x40;
                }
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n",
                    end - begin,
                    begin,
                    end - 1,
                    bytes.len()
                );
                let _ = socket.write_all(header.as_bytes());
                if begin >= raw_start {
                    if let Some(cancel) = cancel_at.lock().unwrap().as_ref() {
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
            stop,
            worker: Some(worker),
        }
    }
    fn spec(&self, cap: usize) -> SourceSpec {
        serde_json::from_value(json!({"location":self.url,"http":{
            "allow_http":true,"cache_bytes":0,"max_range_bytes":cap,
            "max_requests":256,"max_download_bytes":32 << 20,"timeout_seconds":3
        }}))
        .unwrap()
    }
    fn raw(&self) -> Vec<Range> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.begin >= self.raw_start)
            .cloned()
            .collect()
    }
    fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn sample(band: usize, cell: usize) -> f32 {
    match cell {
        5 => f32::from_bits(0x7fc0_0001 + band as u32),
        6 => -0.,
        7 => f32::INFINITY,
        9 => -9999.,
        _ => (band * 10000 + cell) as f32,
    }
}
fn mask(band: usize, cell: usize) -> u8 {
    [0, 127, 255, 1][(cell + band) % 4]
}
fn address(id: usize) -> usize {
    BOOT + id / 64 * PAGE + 16 + id % 64 * RECORD
}
fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}
fn u64_at(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

struct Fixture {
    _directory: tempfile::TempDir,
    original: PathBuf,
    bands: usize,
    width: usize,
    height: usize,
    bytes: Vec<u8>,
}
impl Fixture {
    fn new(
        bands: usize,
        edge: usize,
        width: usize,
        height: usize,
        codec: &str,
        predictor: &str,
        band_group: usize,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("independent.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<f32, _>(&original, width, height, bands)
            .unwrap();
        ds.set_geo_transform(&[0., 1., 0., height as f64, 0., -1.])
            .unwrap();
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        for b in 0..bands {
            let mut band = ds.rasterband(b + 1).unwrap();
            let values = (0..width * height).map(|i| sample(b, i)).collect();
            band.write(
                (0, 0),
                (width, height),
                &mut Buffer::new((width, height), values),
            )
            .unwrap();
            band.set_no_data_value(Some(-9999.)).unwrap();
            band.set_scale(1.25).unwrap();
            band.set_offset(-(b as f64)).unwrap();
            band.create_mask_band(false).unwrap();
            let masks = (0..width * height).map(|i| mask(b, i)).collect();
            band.open_mask_band()
                .unwrap()
                .write(
                    (0, 0),
                    (width, height),
                    &mut Buffer::new((width, height), masks),
                )
                .unwrap();
        }
        ds.flush_cache().unwrap();
        drop(ds);
        let source = open_source(
            &serde_json::from_value(json!({"location":original})).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let destination = directory.path().join("independent.skv");
        let receipt = compile(
            source.as_ref(),
            destination.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: edge,
                band_group,
                codec: codec.into(),
                predictor: predictor.into(),
                payload_layout: "row_group_v1".into(),
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(receipt["payload_layout"], "row_group_v1");
        let bytes = fs::read(destination).unwrap();
        assert_eq!(u32_at(&bytes, 12) & 8, 8);
        Self {
            _directory: directory,
            original,
            bands,
            width,
            height,
            bytes,
        }
    }
    fn interval(&self, tile: usize, band: usize) -> (usize, usize) {
        let at = address(tile * self.bands + band);
        let begin = u64_at(&self.bytes, at);
        (begin, begin + u32_at(&self.bytes, at + 8))
    }
    fn assert_read(
        &self,
        source: &dyn WindowSource,
        bands: &[usize],
        window: [usize; 4],
    ) -> RawWindow {
        let [x, y, w, h] = window;
        let bound = source.raw_read_buffer_bound(w, h, bands).unwrap();
        assert_eq!(bound, w * h * bands.len() * 5 + (8 << 20));
        let (actual, _) = source
            .read_raw_selected_window_cancellable(x, y, w, h, bands, bound, &AtomicBool::new(false))
            .unwrap();
        assert_eq!((actual.width, actual.height), (w, h));
        assert_eq!(actual.bands.len(), bands.len());
        for (got, &logical) in actual.bands.iter().zip(bands) {
            let original = source.raw_metadata().unwrap().bands[logical].original_band_index;
            for row in 0..h {
                for col in 0..w {
                    let i = row * w + col;
                    let cell = (y + row) * self.width + x + col;
                    assert_eq!(
                        &got.samples_le[i * 4..i * 4 + 4],
                        &sample(original, cell).to_le_bytes()
                    );
                    assert_eq!(got.mask[i], mask(original, cell));
                }
            }
        }
        assert!(source.retained_memory_bound() <= 16 << 20);
        assert_eq!(source.diagnostics()["remote"]["cache_capacity_bytes"], 0);
        actual
    }
    fn assert_ranges(&self, requests: &[Range], tiles: &[usize], bands: &[usize]) {
        let mut expected = tiles
            .iter()
            .flat_map(|&tile| bands.iter().map(move |&band| self.interval(tile, band)))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        expected.dedup(); // Multiple selected members legitimately alias one packet.
        fn merged(mut intervals: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
            intervals.sort_unstable();
            let mut output: Vec<(usize, usize)> = Vec::new();
            for (begin, end) in intervals {
                if let Some(previous) = output.last_mut() {
                    assert!(
                        begin >= previous.1,
                        "packets must not be fetched twice in one source read"
                    );
                    if begin == previous.1 {
                        previous.1 = end;
                        continue;
                    }
                }
                output.push((begin, end));
            }
            output
        }
        assert_eq!(
            merged(requests.iter().map(|r| (r.begin, r.end)).collect()),
            merged(expected),
            "exact required packet set, including explicit within-packet amplification only"
        );
    }
}

#[test]
fn generated_36_and_40_band_groups_preserve_bits_masks_mapping_and_adjacent_packet_bytes() {
    for (bands, codec, predictor) in [(36, "none", "none"), (40, "deflate", "byte_delta_v1")] {
        let fixture = Fixture::new(bands, 64, 64 * 3 - 9, 53, codec, predictor, bands);
        // Serving must be independent of the original TIFF and mask sidecars.
        fs::rename(
            &fixture.original,
            fixture.original.with_extension("unavailable"),
        )
        .unwrap();
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
        let selected = (0..bands).map(|i| (i * 17) % bands).collect::<Vec<_>>();
        fixture.assert_read(&source, &selected, [0, 0, fixture.width, fixture.height]);
        let reads = server.raw();
        assert_eq!(
            reads.len(),
            1,
            "three consecutive required packets fit one bounded GET"
        );
        fixture.assert_ranges(&reads, &[0, 1, 2], &selected);
        assert!(reads[0].len() <= MAX_GROUP);
        let before = reads[0].len();
        server.clear();
        fixture.assert_read(&source, &selected, [0, 0, fixture.width, fixture.height]);
        assert_eq!(
            server.raw().len(),
            1,
            "cache0 repeated reads remain physical reads"
        );
        assert_eq!(server.raw()[0].len(), before);
        server.clear();
        let mut mapped_spec = server.spec(4 << 20);
        mapped_spec.bands = Some(vec![bands - 1, 0, 17]);
        let mapped = SkvSource::open(&mapped_spec, &AtomicBool::new(false)).unwrap();
        fixture.assert_read(
            &mapped,
            &[2, 0, 1],
            [1, 1, fixture.width - 2, fixture.height - 2],
        );
        fixture.assert_ranges(&server.raw(), &[0, 1, 2], &[17, bands - 1, 0]);
    }
}

#[test]
fn sparse_selection_amplification_is_exact_and_unselected_packets_are_never_prefetched() {
    let fixture = Fixture::new(40, 64, 192, 128, "none", "none", 40);
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    let one = fixture.assert_read(&source, &[17], [0, 0, 64, 64]);
    assert_eq!(server.raw().len(), 1);
    let returned = one.bands[0].samples_le.len() + one.bands[0].mask.len();
    assert_eq!(
        server.raw()[0].len(),
        40 * returned,
        "one selected F32 band requires all40 packet members; report this cost"
    );
    fixture.assert_ranges(&server.raw(), &[0], &[17]);
    server.clear();
    fixture.assert_read(&source, &[39, 0, 17], [0, 0, 64, 128]);
    assert_eq!(
        server.raw().len(),
        2,
        "the other two tile columns are raw gaps"
    );
    fixture.assert_ranges(&server.raw(), &[0, 3], &[0, 17, 39]);

    // Three physical band groups: members of the middle group remain unneeded.
    let split = Fixture::new(40, 64, 128, 64, "none", "none", 16);
    let server = Server::new(split.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    split.assert_read(&source, &[39, 0], [0, 0, 128, 64]);
    split.assert_ranges(&server.raw(), &[0, 1], &[0, 39]);
}

#[test]
fn grouped_packet_and_output_budgets_reject_before_raw_io_and_caps_split_only_at_packets() {
    let fixture = Fixture::new(40, 128, 256, 128, "none", "none", 40);
    let selected = (0..40).collect::<Vec<_>>();
    assert_eq!(
        fixture.interval(0, 0).1 - fixture.interval(0, 0).0,
        MAX_GROUP
    );
    for cap in [MAX_GROUP, 4 << 20] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(cap), &AtomicBool::new(false)).unwrap();
        fixture.assert_read(&source, &selected, [0, 0, 256, 128]);
        assert_eq!(server.raw().len(), 2);
        assert!(
            server
                .raw()
                .iter()
                .all(|r| r.len() <= cap && r.len() <= MAX_GROUP)
        );
        fixture.assert_ranges(&server.raw(), &[0, 1], &selected);
    }
    for (cap, budget) in [
        (MAX_GROUP - 1, 16 << 20),
        (4 << 20, (8 << 20) + 128 * 128 * 5 - 1),
    ] {
        let server = Server::new(fixture.bytes.clone());
        let source = SkvSource::open(&server.spec(cap), &AtomicBool::new(false)).unwrap();
        let error = source
            .read_raw_selected_window_cancellable(
                0,
                0,
                128,
                128,
                &[39],
                budget,
                &AtomicBool::new(false),
            )
            .err()
            .expect("budget must reject");
        assert!(
            error.to_string().contains("budget")
                || error.to_string().contains("bound")
                || error.to_string().contains("range limit"),
            "{error:#}"
        );
        assert!(
            server.raw().is_empty(),
            "an oversized packet/output cannot begin raw transport"
        );
    }
}

fn mutate_record(bytes: &mut [u8], id: usize, change: impl FnOnce(&mut [u8])) {
    let at = address(id);
    change(&mut bytes[at..at + RECORD]);
    let page = BOOT + id / 64 * PAGE;
    let hash = blake3::hash(&bytes[page..page + PAGE - 32]);
    bytes[page + PAGE - 32..page + PAGE].copy_from_slice(hash.as_bytes());
}

#[test]
fn forged_nonleader_aliases_reject_with_valid_page_checksums_and_quarantine_the_handle() {
    let fixture = Fixture::new(40, 64, 128, 64, "none", "none", 40);
    for mutation in 0..4 {
        let mut bytes = fixture.bytes.clone();
        let second = fixture.interval(1, 0).0 as u64;
        mutate_record(&mut bytes, 17, |record| match mutation {
            0 => record[112..120].copy_from_slice(&40u64.to_le_bytes()),
            1 => record[120..124].copy_from_slice(&39u32.to_le_bytes()),
            2 => record[0..8].copy_from_slice(&second.to_le_bytes()),
            _ => record[32] ^= 0x40,
        });
        let server = Server::new(bytes);
        let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
        assert!(
            source
                .read_raw_selected_window_cancellable(
                    0,
                    0,
                    64,
                    64,
                    &[17],
                    16 << 20,
                    &AtomicBool::new(false)
                )
                .is_err(),
            "forgery {mutation}"
        );
        assert!(
            server.raw().is_empty(),
            "invalid canonical alias must fail before payload transport"
        );
        assert_eq!(source.diagnostics()["invalidated"], true);
        let reads = server.requests.lock().unwrap().len();
        assert!(
            source
                .read_raw_selected_window_cancellable(
                    0,
                    0,
                    1,
                    1,
                    &[0],
                    16 << 20,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(server.requests.lock().unwrap().len(), reads);
    }
}

#[test]
fn coherent_distinct_canonical_groups_cannot_claim_the_same_physical_interval() {
    let fixture = Fixture::new(40, 64, 128, 64, "none", "none", 40);
    let mut bytes = fixture.bytes.clone();
    let (begin, end) = fixture.interval(0, 0);
    let second_leader = 40usize;
    let second_at = address(second_leader);
    // Forge an internally coherent second group: every alias agrees, and its
    // payload hash binds the second leader to the first group's actual bytes.
    // Only the global physical-interval uniqueness rule distinguishes it.
    let mut hash = blake3::Hasher::new();
    hash.update(b"SKV-row-group-v1\0");
    hash.update(&(second_leader as u64).to_le_bytes());
    hash.update(&40u32.to_le_bytes());
    hash.update(&bytes[second_at + 12..second_at + 24]);
    hash.update(&bytes[begin..end]);
    let digest = hash.finalize();
    for id in second_leader..second_leader + 40 {
        mutate_record(&mut bytes, id, |record| {
            record[..8].copy_from_slice(&(begin as u64).to_le_bytes());
            record[32..64].copy_from_slice(digest.as_bytes());
        });
    }
    // Keep the complete directory and bootstrap checksums coherent too, even
    // though cold reads intentionally do not inspect the entire directory.
    let metadata_len = u32_at(&bytes, 24);
    let mut metadata: Value =
        serde_json::from_reader(ZlibDecoder::new(&bytes[64..64 + metadata_len])).unwrap();
    let data_offset = u64_at(&bytes, 56);
    metadata["directory_digest"] = blake3::hash(&bytes[BOOT..data_offset])
        .to_hex()
        .to_string()
        .into();
    let decoded = serde_json::to_vec(&metadata).unwrap();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(3));
    encoder.write_all(&decoded).unwrap();
    let encoded = encoder.finish().unwrap();
    assert!(encoded.len() <= BOOT - 96);
    bytes[24..28].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
    bytes[28..32].copy_from_slice(&(decoded.len() as u32).to_le_bytes());
    bytes[64..BOOT - 32].fill(0);
    bytes[64..64 + encoded.len()].copy_from_slice(&encoded);
    let checksum = blake3::hash(&bytes[..BOOT - 32]);
    bytes[BOOT - 32..BOOT].copy_from_slice(checksum.as_bytes());

    let server = Server::new(bytes);
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    let error = source
        .read_raw_selected_window_cancellable(
            0,
            0,
            128,
            64,
            &[0, 39],
            16 << 20,
            &AtomicBool::new(false),
        )
        .err()
        .expect("distinct canonical groups must have disjoint physical payloads");
    assert!(
        error
            .to_string()
            .contains("overlapping SKV payload records"),
        "{error:#}"
    );
    assert!(server.raw().is_empty());
    assert_eq!(source.diagnostics()["invalidated"], true);
    let before = server.requests.lock().unwrap().len();
    assert!(
        source
            .read_raw_selected_window_cancellable(
                0,
                0,
                1,
                1,
                &[0],
                16 << 20,
                &AtomicBool::new(false),
            )
            .is_err()
    );
    assert_eq!(server.requests.lock().unwrap().len(), before);
}

#[test]
fn corruption_in_unselected_nonleader_bytes_and_mid_read_cancellation_are_sticky() {
    let fixture = Fixture::new(40, 64, 192, 64, "none", "none", 40);
    for cancellation in [false, true] {
        let server = Server::new(fixture.bytes.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let source = SkvSource::open(&server.spec(4 << 20), &cancel).unwrap();
        if cancellation {
            *server.cancel_raw.lock().unwrap() = Some(cancel.clone());
        } else {
            // row0/plane0/x0/member39 is unselected, but shares packet integrity.
            server
                .corrupt
                .store(fixture.interval(2, 0).0 + 39, Ordering::Release);
        }
        let error = source
            .read_raw_selected_window_cancellable(0, 0, 192, 64, &[0], 16 << 20, &cancel)
            .err()
            .expect("complete source read must fail");
        if !cancellation {
            assert!(error.to_string().contains("checksum"), "{error:#}");
        }
        assert_eq!(server.raw().len(), 1);
        assert_eq!(source.diagnostics()["invalidated"], true);
        let reads = server.requests.lock().unwrap().len();
        cancel.store(false, Ordering::Release);
        server.corrupt.store(usize::MAX, Ordering::Release);
        assert!(
            source
                .read_raw_selected_window_cancellable(0, 0, 1, 1, &[0], 16 << 20, &cancel)
                .is_err()
        );
        assert_eq!(server.requests.lock().unwrap().len(), reads);
    }
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({"type":"Polygon","coordinates":[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]})
}
fn single(source: &dyn WindowSource, zone: &Value, options: &Options) -> Value {
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
fn batch(source: &dyn WindowSource, zones: &[Value], options: &Options) -> Value {
    let spec:JobSpec=serde_json::from_value(json!({
        "zones":zones.iter().enumerate().map(|(i,z)|json!({"id":i.to_string(),"version":"1","geometry":z})).collect::<Vec<_>>(),
        "slices":[{"id":"one","source":"r"}],"crs":"EPSG:3857","tile_edge":64,
        "options":options,"budget":{"tile_bytes":64<<20}
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
fn grouped_native_single_and_shared_batch_use_summaries_without_fetching_interior_packets() {
    let fixture = Fixture::new(40, 64, 128, 64, "deflate", "byte_delta_v1", 40);
    let original = open_source(
        &serde_json::from_value(json!({"location":fixture.original})).unwrap(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let options = Options {
        bands: (0..40).rev().collect(),
        statistics: Some(
            ["sum", "support", "mean", "min", "max"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..Default::default()
    };
    let zones = [
        rectangle(-1., -1., 95.5, 65.),
        rectangle(70.25, 2., 70.250000001, 60.),
    ];
    let expected = zones
        .iter()
        .map(|z| single(original.as_ref(), z, &options))
        .collect::<Vec<_>>();
    let reference = batch(original.as_ref(), &zones, &options);
    let server = Server::new(fixture.bytes.clone());
    let source = SkvSource::open(&server.spec(4 << 20), &AtomicBool::new(false)).unwrap();
    for (zone, want) in zones.iter().zip(&expected) {
        server.clear();
        assert_eq!(single(&source, zone, &options)["bands"], want["bands"]);
        assert_eq!(server.raw().len(), 1);
        fixture.assert_ranges(&server.raw(), &[1], &options.bands);
    }
    assert!(
        expected[1]["bands"][0]["covered_cell_equivalents"]
            .as_f64()
            .unwrap()
            > 0.
    );
    server.clear();
    let actual = batch(&source, &zones, &options);
    assert_eq!(actual["complete"], true);
    for i in 0..zones.len() {
        assert_eq!(actual["rows"][i]["bands"], reference["rows"][i]["bands"]);
    }
    assert_eq!(server.raw().len(), 1);
    fixture.assert_ranges(&server.raw(), &[1], &options.bands);
}
