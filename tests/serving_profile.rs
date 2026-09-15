use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use raster_engine::{
    io::open_source,
    ordered_source::{OrderedRequest, POLICY},
    serving_profile::{
        self, expected_view, expected_view_sha256, interpretation_sha256, selector_sha256,
    },
    session::{Session, request},
    skv::{CompileOptions, compile},
    source::SourceSpec,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Server {
    url: String,
    calls: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bytes: Vec<u8>, suffix: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/source.{suffix}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, stopped) = (calls.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
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
                let mut data = Vec::new();
                let mut byte = [0];
                while !data.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    data.push(byte[0]);
                    assert!(data.len() < 16384);
                }
                let text = String::from_utf8(data).unwrap().to_ascii_lowercase();
                observed
                    .lock()
                    .unwrap()
                    .push(text.lines().next().unwrap().to_owned());
                if text.starts_with("head ") {
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixed\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
                } else {
                    assert!(text.starts_with("get "));
                    // Known SKV registration now sends a declared expected
                    // generation on its first GET. Model provider rejection.
                    if text.contains("if-match:") && !text.contains("if-match: \"fixed\"") {
                        let _ = write!(
                            socket,
                            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        continue;
                    }
                    assert!(
                        text.contains("if-match: \"fixed\"")
                            || text.lines().any(|line| line == "range: bytes=0-16383")
                    );
                    let range = text
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .unwrap();
                    let (a, b) = range.trim().split_once('-').unwrap();
                    let begin = a.parse::<usize>().unwrap();
                    let end = b.parse::<usize>().unwrap() + 1;
                    assert!(begin < end && end <= bytes.len());
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n",
                        end - begin,
                        begin,
                        end - 1,
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes());
                    let _ = socket.write_all(&bytes[begin..end]);
                }
            }
        });
        Self {
            url,
            calls,
            stop,
            worker: Some(worker),
        }
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    direct: PathBuf,
    accelerated: PathBuf,
    profile: Value,
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let direct = dir.path().join("source.tif");
        let accelerated = dir.path().join("source.skv");
        let mut ds = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<f32, _>(&direct, 8, 4, 2)
            .unwrap();
        ds.set_geo_transform(&[0., 1., 0., 4., 0., -1.]).unwrap();
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        for b in 0..2 {
            let mut values = (0..32).map(|i| (i + b * 32) as f32).collect::<Vec<_>>();
            values[0] = -0.;
            values[1] = -9999.;
            values[2] = -2.;
            values[3] = f32::from_bits(0x7fc01234 + b as u32);
            let mut band = ds.rasterband(b + 1).unwrap();
            band.set_no_data_value(Some(-9999.)).unwrap();
            band.set_scale(2.).unwrap();
            band.set_offset(3.).unwrap();
            band.write((0, 0), (8, 4), &mut Buffer::new((8, 4), values))
                .unwrap();
            // Keep the portable content-identity source self-contained. GDAL's
            // declared NoData supplies a reproducible zero/255 mask here.
        }
        ds.flush_cache().unwrap();
        drop(ds);
        let cancel = AtomicBool::new(false);
        let raw_source = open_source(
            &serde_json::from_value(json!({"location":direct})).unwrap(),
            &cancel,
        )
        .unwrap();
        compile(
            raw_source.as_ref(),
            accelerated.to_str().unwrap(),
            &CompileOptions {
                chunk_edge: 64,
                band_group: 2,
                payload_layout: "row_group_v1".into(),
                predictor: "byte_delta_v1".into(),
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let skv_source = open_source(
            &serde_json::from_value(json!({"location":accelerated,"format":"skv"})).unwrap(),
            &cancel,
        )
        .unwrap();
        let (raw, _) = raw_source
            .read_raw_selected_window_cancellable(0, 0, 8, 4, &[0, 1], 32 << 20, &cancel)
            .unwrap();
        let (copy, _) = skv_source
            .read_raw_selected_window_cancellable(0, 0, 8, 4, &[0, 1], 32 << 20, &cancel)
            .unwrap();
        let mut samples = Sha256::new();
        let mut masks = Sha256::new();
        for (a, b) in raw.bands.iter().zip(&copy.bands) {
            assert_eq!(a.samples_le, b.samples_le);
            assert_eq!(a.mask, b.mask);
            samples.update(&a.samples_le);
            masks.update(&a.mask);
        }
        let direct_expected = expected_view(
            &raw_source.metadata().grid,
            raw_source.raw_metadata().unwrap(),
        );
        let accelerated_expected = expected_view(
            &skv_source.metadata().grid,
            skv_source.raw_metadata().unwrap(),
        );
        assert_eq!(
            interpretation_sha256(&direct_expected).unwrap(),
            interpretation_sha256(&accelerated_expected).unwrap()
        );
        let direct_bytes = fs::read(&direct).unwrap();
        let accelerated_bytes = fs::read(&accelerated).unwrap();
        let direct_spec = json!({"location":direct,"format":"geotiff","identity":{"sha256":sha(&direct_bytes),"byte_length":direct_bytes.len(),"policy":"verify"},"http":{"cache_bytes":0}});
        let accelerated_spec = json!({"location":accelerated,"format":"skv","identity":{"sha256":sha(&accelerated_bytes),"byte_length":accelerated_bytes.len(),"policy":"verify"},"http":{"cache_bytes":0}});
        let pin = |spec: &Value, expected: &serving_profile::ExpectedView| {
            json!({
            "sha256":spec["identity"]["sha256"],"byte_length":spec["identity"]["byte_length"],
            "selector_sha256":selector_sha256(&serde_json::from_value::<SourceSpec>(spec.clone()).unwrap()).unwrap(),
            "expected_view_sha256":expected_view_sha256(expected).unwrap()})
        };
        let profile = json!({"schema":serving_profile::SCHEMA,"profile_id":"generated-pair-v1","view_id":"generated-native-8x4-v1","numerical_policy":POLICY,
            "direct":{"spec":direct_spec,"expected":direct_expected},"accelerated":{"spec":accelerated_spec,"expected":accelerated_expected},
            "attestation":{"kind":"owner_verified_full_view_v1","verifier":"test-full-bitwise-v1","receipt_sha256":sha(b"test-full-bitwise-v1"),
                "checks":["typed_sample_bits","mask_bytes","full_interpretation"],"samples_sha256":format!("{:x}",samples.finalize()),
                "masks_sha256":format!("{:x}",masks.finalize()),"interpretation_sha256":interpretation_sha256(&direct_expected).unwrap(),"cells_per_band":32,
                "direct":pin(&direct_spec,&direct_expected),"accelerated":pin(&accelerated_spec,&accelerated_expected)},
            "accelerated_when":{"enabled":true,"access_classes":["local","controlled"],"bands":[0,1],"polygons":[1,2],"windows":[1,4],
                "selected_cells":[1,64],"envelope_width":[1,8],"envelope_height":[1,4],"envelope_cells":[1,32]}});
        Self {
            _dir: dir,
            direct,
            accelerated,
            profile,
        }
    }
    fn remote(&self) -> (Value, Server, Server) {
        let direct = Server::new(fs::read(&self.direct).unwrap(), "tif");
        let accelerated = Server::new(fs::read(&self.accelerated).unwrap(), "skv");
        let mut profile = self.profile.clone();
        for (role, server) in [("direct", &direct), ("accelerated", &accelerated)] {
            profile[role]["spec"]["location"] = server.url.clone().into();
            profile[role]["spec"]["identity"]["policy"] = "trusted_manifest".into();
            profile[role]["spec"]["identity"]["etag"] = "\"fixed\"".into();
            profile[role]["spec"]["http"] = json!({"allow_http":true,"cache_bytes":0,"max_requests":64,"max_range_bytes":4<<20,"max_download_bytes":8<<20,"timeout_seconds":3});
        }
        (profile, direct, accelerated)
    }
}
fn selections() -> Value {
    json!({"bands":[1,0],"polygons":[{"id":"a","windows":[{"window":[0,0,8,4],"runs":[[0,5],[8,16],[25,32]]}]}]})
}
fn command(profile: &Value, selections: &Value, access: &str) -> Value {
    json!({"op":"measure_ordered_profile","profile":profile,"request":selections,
    "numerical_policy":POLICY,"view_id":"generated-native-8x4-v1","access_class":access})
}
fn call(session: &mut Session, value: Value) -> Value {
    serde_json::from_str(&request(
        session,
        &value.to_string(),
        &AtomicBool::new(false),
    ))
    .unwrap()
}
fn result(session: &mut Session, value: Value) -> Value {
    let r = call(session, value);
    assert_eq!(r["ok"], true, "{r}");
    r["result"].clone()
}

#[test]
fn actual_request_routes_one_source_and_preserves_permuted_ordered_answers_without_handles() {
    let fixture = Fixture::new();
    let (profile, direct, accelerated) = fixture.remote();
    let mut session = Session::default();
    let baseline = session.bytes();
    let all = selections();
    let a = result(&mut session, command(&profile, &all, "controlled"));
    assert_eq!(a["routing"]["selected"], "accelerated");
    assert_eq!(direct.count(), 0);
    assert!(accelerated.count() > 0);
    assert_eq!(a["routing"]["facts"]["selected_bands"], json!([1, 0]));
    assert_eq!(a["routing"]["facts"]["selected_cells"], 20);
    assert_eq!(a["routing"]["source_closed"], true);
    assert_eq!(session.bytes(), baseline);
    assert!(
        a["routing"]["complete_profile_ms"].as_f64().unwrap()
            >= a["routing"]["source_open_and_check_ms"].as_f64().unwrap()
    );
    direct.clear();
    accelerated.clear();
    let b = result(&mut session, command(&profile, &all, "unknown"));
    assert_eq!(b["routing"]["selected"], "direct");
    assert!(direct.count() > 0);
    assert_eq!(accelerated.count(), 0);
    assert_eq!(a["rows"], b["rows"]);
    direct.clear();
    accelerated.clear();
    let mut one = all.clone();
    one["bands"] = json!([1]);
    let c = result(&mut session, command(&profile, &one, "controlled"));
    assert_eq!(c["routing"]["selected"], "direct");
    assert!(direct.count() > 0);
    assert_eq!(accelerated.count(), 0);
    assert_eq!(c["rows"][0]["bands"][0], a["rows"][0]["bands"][0]);
    direct.clear();
    accelerated.clear();
    let mut implicit = all;
    implicit.as_object_mut().unwrap().remove("bands");
    let d = result(&mut session, command(&profile, &implicit, "controlled"));
    assert_eq!(d["routing"]["selected"], "accelerated");
    assert_eq!(d["rows"][0]["bands"][0], a["rows"][0]["bands"][1]);
    assert_eq!(direct.count(), 0);
    assert!(accelerated.count() > 0);
    assert_eq!(session.bytes(), baseline);
    let serialized = a["routing"].to_string();
    assert!(!serialized.contains(&direct.url));
    assert!(!serialized.contains(&accelerated.url));
}

#[test]
fn explicit_access_and_every_inclusive_rule_bound_are_conjunctive() {
    let fixture = Fixture::new();
    let mut session = Session::default();
    let request = selections();
    for (field, value) in [
        ("enabled", json!(false)),
        ("access_classes", json!(["other"])),
        ("bands", json!([0])),
        ("polygons", json!([2, 2])),
        ("windows", json!([2, 2])),
        ("selected_cells", json!([21, 64])),
        ("envelope_width", json!([1, 7])),
        ("envelope_height", json!([1, 3])),
        ("envelope_cells", json!([1, 31])),
    ] {
        let mut profile = fixture.profile.clone();
        profile["accelerated_when"][field] = value;
        assert_eq!(
            result(&mut session, command(&profile, &request, "local"))["routing"]["selected"],
            "direct",
            "{field}"
        );
    }
    let mut edge = fixture.profile.clone();
    for (field, value) in [
        ("polygons", json!([1, 1])),
        ("windows", json!([1, 1])),
        ("selected_cells", json!([20, 20])),
        ("envelope_width", json!([8, 8])),
        ("envelope_height", json!([4, 4])),
        ("envelope_cells", json!([32, 32])),
    ] {
        edge["accelerated_when"][field] = value;
    }
    assert_eq!(
        result(&mut session, command(&edge, &request, "local"))["routing"]["selected"],
        "accelerated"
    );
    let mut omitted = command(&edge, &request, "local");
    omitted.as_object_mut().unwrap().remove("access_class");
    assert_eq!(
        result(&mut session, omitted)["routing"]["selected"],
        "direct"
    );
}

#[test]
fn invalid_policy_view_attestation_fields_selectors_and_requests_fail_before_any_source_open() {
    let fixture = Fixture::new();
    let (profile, direct, accelerated) = fixture.remote();
    let base = command(&profile, &selections(), "controlled");
    let mut bad = Vec::new();
    for field in ["view_id", "numerical_policy"] {
        let mut v = base.clone();
        v[field] = "different".into();
        bad.push(v);
    }
    for field in ["backend", "mode", "surprise"] {
        let mut v = base.clone();
        v[field] = "ignored?".into();
        bad.push(v);
    }
    for path in ["schema", "view_id"] {
        let mut v = base.clone();
        v["profile"][path] = "wrong".into();
        bad.push(v);
    }
    let mut v = base.clone();
    v["profile"]["attestation"]["checks"] = json!(["typed_sample_bits", "full_interpretation"]);
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["attestation"]["receipt_sha256"] = "not-a-hash".into();
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["direct"]["spec"]["identity"]["sha256"] = "0".repeat(64).into();
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["accelerated"]["spec"]["bands"] = json!([1, 0]);
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["accelerated"]["expected"]["raw_metadata"]["bands"][0]["scale_f64_bits"] =
        json!(4611686018427387904u64);
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["direct"]["expected"]["raw_metadata"]["bands"][0]["unknown"] = true.into();
    bad.push(v);
    let mut v = base.clone();
    v["profile"]["padding"] = "x".repeat(65536).into();
    bad.push(v);
    let mut v = base.clone();
    v["request"]["bands"] = json!([0, 0]);
    bad.push(v);
    let mut v = base.clone();
    v["request"]["nodata"] = json!([-9999., -9999.]);
    bad.push(v);
    let mut v = base.clone();
    v["request"]["polygons"][0]["windows"][0]["window"] = json!([1, 0, 8, 4]);
    bad.push(v);
    let mut v = base.clone();
    v["request"]["polygons"][0]["windows"][0]["runs"] = json!([[8, 10], [1, 3]]);
    bad.push(v);
    let mut v = base.clone();
    v["request"]["polygons"][0]["windows"][0]["indexes"] = json!([1, 2]);
    bad.push(v);
    let mut session = Session::default();
    for (i, v) in bad.into_iter().enumerate() {
        let r = call(&mut session, v);
        assert_eq!(r["ok"], false, "case{i}: {r}");
        assert_eq!(direct.count() + accelerated.count(), 0, "case{i}");
    }
}

#[test]
fn opened_view_is_checked_against_proof_and_failure_never_opens_the_other_candidate() {
    let fixture = Fixture::new();
    let (mut profile, direct, accelerated) = fixture.remote();
    // A coherent owner expectation can still be wrong. Both candidates have the
    // same forged interpretation, but actual source metadata must be checked.
    for role in ["direct", "accelerated"] {
        profile[role]["expected"]["raw_metadata"]["bands"][0]["scale_f64_bits"] =
            "4008000000000000".into();
        let expected: serving_profile::ExpectedView =
            serde_json::from_value(profile[role]["expected"].clone()).unwrap();
        profile["attestation"][role]["expected_view_sha256"] =
            expected_view_sha256(&expected).unwrap().into();
        profile["attestation"]["interpretation_sha256"] =
            interpretation_sha256(&expected).unwrap().into();
    }
    let mut session = Session::default();
    let r = call(&mut session, command(&profile, &selections(), "controlled"));
    assert_eq!(r["ok"], false);
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("opened source interpretation")
    );
    assert_eq!(direct.count(), 0);
    assert!(accelerated.count() > 0);
    let (mut profile, direct, accelerated) = fixture.remote();
    profile["accelerated"]["spec"]["identity"]["etag"] = "\"changed\"".into();
    let r = call(&mut session, command(&profile, &selections(), "controlled"));
    assert_eq!(r["ok"], false);
    assert_eq!(direct.count(), 0);
    assert!(accelerated.count() > 0);
}

#[test]
fn bit_safe_provisioning_view_and_canonical_hashes_preserve_source_metadata() {
    let fixture = Fixture::new();
    let mut session = Session::default();
    result(
        &mut session,
        json!({"op":"register_source","id":"r","spec":{"location":fixture.direct}}),
    );
    let info = result(&mut session, json!({"op":"source_info","source":"r"}));
    assert_eq!(
        info["serving_profile_view"],
        fixture.profile["direct"]["expected"]
    );
    assert_eq!(
        info["serving_profile_view"]["raw_metadata"]["bands"][0]["scale_f64_bits"],
        "4000000000000000"
    );
    let a = json!({"z":[{"b":2,"a":1}],"a":"x"});
    assert_eq!(
        serving_profile::canonical_sha256(&a).unwrap(),
        sha(br#"{"a":"x","z":[{"a":1,"b":2}]}"#)
    );
    let source = open_source(
        &serde_json::from_value(json!({"location":fixture.direct})).unwrap(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut view = expected_view(&source.metadata().grid, source.raw_metadata().unwrap());
    let positive = expected_view_sha256(&view).unwrap();
    view.grid.transform_f64_bits[0] = "8000000000000000".into();
    assert_ne!(positive, expected_view_sha256(&view).unwrap());
}

#[test]
fn cancellation_and_resource_rejection_do_not_open_sources_or_retain_handles() {
    let fixture = Fixture::new();
    let (profile, direct, accelerated) = fixture.remote();
    let selected: OrderedRequest = serde_json::from_value(selections()).unwrap();
    for (available, cancel) in [(1 << 20, false), (1 << 30, true)] {
        assert!(
            serving_profile::execute(
                &profile,
                &selected,
                POLICY,
                "generated-native-8x4-v1",
                Some("controlled"),
                available,
                &AtomicBool::new(cancel)
            )
            .is_err()
        );
        assert_eq!(direct.count() + accelerated.count(), 0);
    }
    let mut session = Session::default();
    let before = session.bytes();
    let mut command = command(&profile, &selections(), "controlled");
    command["request"]["budget"] = json!({"max_contributions":1});
    assert_eq!(call(&mut session, command)["ok"], false);
    assert_eq!(direct.count() + accelerated.count(), 0);
    assert_eq!(session.bytes(), before);
}
