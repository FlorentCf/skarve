use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    io::open_source_for_compile,
    skv::{CompileOptions, SkvSource, compile},
    source::{SourceSpec, WindowSource},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

fn spec(path: &Path) -> SourceSpec {
    serde_json::from_value(json!({"location":path})).unwrap()
}
fn fixture(path: &Path) {
    let mut ds = DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<f32, _>(path, 4, 2, 2)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 2., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for i in 1..=2 {
        ds.rasterband(i)
            .unwrap()
            .write(
                (0, 0),
                (4, 2),
                &mut Buffer::new((4, 2), (1..=8).map(|v| (v * i) as f32).collect()),
            )
            .unwrap();
    }
    ds.flush_cache().unwrap();
}
fn compiled(dir: &Path) -> (std::path::PathBuf, Value) {
    let tif = dir.join("input.tif");
    fixture(&tif);
    let path = dir.join("output.skv");
    let cancel = AtomicBool::new(false);
    let input = open_source_for_compile(&spec(&tif), &cancel).unwrap();
    let receipt = compile(
        input.as_ref(),
        path.to_str().unwrap(),
        &CompileOptions::default(),
        &cancel,
    )
    .unwrap();
    (path, receipt)
}
#[test]
fn source_identity_pins_generation_mapping_and_explicit_content_authority() {
    let dir = tempfile::tempdir().unwrap();
    let (first, receipt) = compiled(dir.path());
    let second = dir.path().join("copy.skv");
    fs::copy(&first, &second).unwrap();
    let cancel = AtomicBool::new(false);
    let a = SkvSource::open(&spec(&first), &cancel).unwrap();
    let b = SkvSource::open(&spec(&second), &cancel).unwrap();
    assert_ne!(
        a.metadata().source_id,
        b.metadata().source_id,
        "default identity must pin the actual serving generation"
    );
    let default = a.identity_descriptor().unwrap();
    assert_eq!(default["identity_authority"], "serving_object_generation");
    assert_eq!(
        default["bootstrap_digest_scope"],
        "bootstrap_only_not_a_merkle_root"
    );
    assert_eq!(default["whole_content_sha256_verified"], false);
    let mapped = |band| {
        serde_json::from_value::<SourceSpec>(json!({"location":first,"bands":[band]})).unwrap()
    };
    let ma = SkvSource::open(&mapped(0), &cancel).unwrap();
    let mb = SkvSource::open(&mapped(1), &cancel).unwrap();
    assert_ne!(ma.metadata().source_id, mb.metadata().source_id);
    for policy in ["verify", "trusted_manifest"] {
        let trusted = |path: &Path| {
            serde_json::from_value::<SourceSpec>(json!({"location":path,"identity":{"sha256":receipt["sha256"],"byte_length":receipt["byte_length"],"policy":policy}})).unwrap()
        };
        let a = SkvSource::open(&trusted(&first), &cancel).unwrap();
        let b = SkvSource::open(&trusted(&second), &cancel).unwrap();
        assert_eq!(a.metadata().source_id, b.metadata().source_id);
        assert_eq!(a.identity_descriptor(), b.identity_descriptor());
        assert_eq!(
            a.identity_descriptor().unwrap()["whole_content_sha256_verified"],
            policy == "verify"
        );
    }
}
#[test]
fn source_owned_cached_summary_rejects_mutation_but_ignores_unrelated_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = compiled(dir.path());
    let cancel = AtomicBool::new(false);
    let source = SkvSource::open(&spec(&path), &cancel).unwrap();
    assert_eq!(
        source
            .stored_summaries()
            .unwrap()
            .read_summary(0, &[0], &cancel)
            .unwrap()[0]
            .valid_count,
        8
    );
    fs::write(
        path.with_file_name("output.skv.msk"),
        b"unrelated mask sibling",
    )
    .unwrap();
    fs::write(
        path.with_file_name("output.skv.aux.xml"),
        b"unrelated PAM sibling",
    )
    .unwrap();
    source.verify_immutable().unwrap();
    assert_eq!(
        source
            .stored_summaries()
            .unwrap()
            .read_summary(0, &[0], &cancel)
            .unwrap()[0]
            .valid_count,
        8
    );
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(b"S").unwrap();
    file.sync_all().unwrap();
    let error = source
        .stored_summaries()
        .unwrap()
        .read_summary(0, &[0], &cancel)
        .err()
        .unwrap();
    assert!(error.to_string().contains("serving object changed"));
    assert_eq!(source.diagnostics()["invalidated"], true);
}
#[test]
fn maximum_cache_fits_the_existing_source_admission() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = compiled(dir.path());
    let cancel = AtomicBool::new(false);
    let spec: SourceSpec =
        serde_json::from_value(json!({"location":path,"http":{"cache_bytes":8<<20}})).unwrap();
    let source = SkvSource::open(&spec, &cancel).unwrap();
    assert!(source.retained_memory_bound() <= raster_engine::source::NATIVE_SOURCE_RETAINED_BYTES);
    #[cfg(feature = "exactextract")]
    {
        use raster_engine::{
            aggregate::Options,
            backend::EeOptions,
            exactextract::{Input, execute},
            tile_cache::TileCache,
        };
        let input = [Input {
            source: &source,
            bands: vec![0, 1],
        }];
        let options = Options {
            statistics: Some(vec!["sum".into()]),
            ..Default::default()
        };
        let zones = [json!({"type":"Polygon","coordinates":[[[0,0],[4,0],[4,2],[0,2],[0,0]]]})];
        let result = execute(
            &input,
            &zones,
            "EPSG:3857",
            &options,
            &EeOptions::default(),
            &cancel,
            &mut TileCache::default(),
            1 << 30,
        )
        .unwrap();
        assert_eq!(result.band_count, 2);
        assert_eq!(
            result.bands(0, 0, options.statistics.as_ref().unwrap())[1]["fractional_sum"],
            72.
        );
    }
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    gets: Arc<AtomicUsize>,
    heads: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/test.skv", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let gets = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(AtomicUsize::new(0));
        let (done, g, h) = (stop.clone(), gets.clone(), heads.clone());
        let thread = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
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
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                let request = String::from_utf8(request).unwrap();
                if request.starts_with("HEAD ") {
                    h.fetch_add(1, Ordering::Relaxed);
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"skv-fixture-v1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
                } else {
                    assert!(request.starts_with("GET "));
                    g.fetch_add(1, Ordering::Relaxed);
                    let lower = request.to_ascii_lowercase();
                    let range = lower
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .unwrap();
                    let (a, b) = range.trim().split_once('-').unwrap();
                    let (a, b) = (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap());
                    assert!(a <= b && b < bytes.len());
                    assert!(
                        lower.contains("if-match: \"skv-fixture-v1\"") || (a == 0 && b == 16383)
                    );
                    write!(socket,"HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"skv-fixture-v1\"\r\nConnection: close\r\n\r\n",b-a+1,a,b,bytes.len()).unwrap();
                    socket.write_all(&bytes[a..=b]).unwrap();
                }
            }
        });
        Self {
            url,
            stop,
            gets,
            heads,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
#[test]
fn controlled_http_counters_follow_the_common_reader_diagnostics_contract() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = compiled(dir.path());
    let server = Server::new(fs::read(path).unwrap());
    let cancel = AtomicBool::new(false);
    let spec: SourceSpec = serde_json::from_value(
        json!({"location":server.url,"http":{"allow_http":true,"max_requests":32}}),
    )
    .unwrap();
    let source = SkvSource::open(&spec, &cancel).unwrap();
    source
        .read_selected_window_cancellable(0, 0, 4, 2, &[0], 16 << 20, &cancel)
        .unwrap();
    source.verify_immutable().unwrap();
    let d = source.diagnostics();
    assert_eq!(d["remote"], d["transport"]["metrics"]);
    assert_eq!(
        d["remote"]["get_requests"],
        server.gets.load(Ordering::Relaxed)
    );
    assert_eq!(
        d["remote"]["head_requests"],
        server.heads.load(Ordering::Relaxed)
    );
    assert!(d["remote"]["received_bytes"].as_u64().unwrap() > 0);
}
