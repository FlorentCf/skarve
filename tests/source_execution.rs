use gdal::{
    DriverManager,
    raster::{Buffer, RasterCreationOptions},
    spatial_ref::SpatialRef,
};
use raster_engine::{
    io::open_source,
    persistent::{self, BoundarySource, PersistedIndex},
    source::SourceSpec,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn fixture(path: &std::path::Path) {
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=128",
        "BLOCKYSIZE=128",
        "COMPRESS=DEFLATE",
    ]);
    let mut ds = driver
        .create_with_band_type_with_options::<f64, _>(path, 256, 256, 3, &opts)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 256., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for i in 1..=3 {
        let mut b = ds.rasterband(i).unwrap();
        b.write(
            (0, 0),
            (256, 256),
            &mut Buffer::new(
                (256, 256),
                (0..256 * 256).map(|n| n as f64 + i as f64 - 100.).collect(),
            ),
        )
        .unwrap();
    }
    ds.rasterband(2).unwrap().set_scale(-2.).unwrap();
    ds.rasterband(2).unwrap().set_offset(3.).unwrap();
    ds.flush_cache().unwrap();
}
fn spec(value: serde_json::Value) -> SourceSpec {
    serde_json::from_value(value).unwrap()
}

#[cfg(unix)]
#[test]
fn sidecar_atomic_replacement_with_preserved_size_and_mtime_invalidates_source() {
    use raster_engine::io::LocalSource;
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::MetadataExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.tif");
    fixture(&path);
    let original = fs::metadata(&path).unwrap();
    let source_without_sidecar = LocalSource::open(path.to_str().unwrap()).unwrap();
    // Independently reproduce the unchanged pre-continuation no-sidecar key.
    let canonical = fs::canonicalize(&path).unwrap();
    let mut digest = Sha256::new();
    digest.update(canonical.as_os_str().as_encoded_bytes());
    digest.update(original.len().to_le_bytes());
    digest.update(format!("{:?}", original.modified().unwrap()).as_bytes());
    digest.update(original.dev().to_le_bytes());
    digest.update(original.ino().to_le_bytes());
    digest.update(original.ctime().to_le_bytes());
    digest.update(original.ctime_nsec().to_le_bytes());
    assert_eq!(
        source_without_sidecar.metadata.source_id,
        format!("local-stat-sha256:{:x}", digest.finalize())
    );
    let sidecar = directory.path().join("source.tif.aux.xml");
    let first = b"<PAMDataset><PAMRasterBand band=\"1\"><UnitType>mm</UnitType></PAMRasterBand></PAMDataset>";
    let second = b"<PAMDataset><PAMRasterBand band=\"1\"><UnitType>cm</UnitType></PAMRasterBand></PAMDataset>";
    assert_eq!(first.len(), second.len());
    fs::write(&sidecar, first).unwrap();
    let source = LocalSource::open(path.to_str().unwrap()).unwrap();
    let before = fs::metadata(&sidecar).unwrap();
    let replacement = directory.path().join("replacement.aux.xml");
    fs::write(&replacement, second).unwrap();
    File::open(&replacement)
        .unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    fs::rename(&replacement, &sidecar).unwrap();
    let after = fs::metadata(&sidecar).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_ne!(before.ino(), after.ino());
    assert!(source.verify_immutable().is_err());
    let reopened = LocalSource::open(path.to_str().unwrap()).unwrap();
    assert_ne!(source.metadata.source_id, reopened.metadata.source_id);
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        original.modified().unwrap()
    );
}

// A tiny independently encoded classic NetCDF file: no fixture-maker dependency.
// Header integers and double payloads use the specified XDR big-endian encoding.
fn netcdf_fixture(path: &std::path::Path, irregular: bool, with_crs: bool, longitude_offset: f64) {
    fn u32b(out: &mut Vec<u8>, value: u32) {
        out.extend(value.to_be_bytes());
    }
    fn name(out: &mut Vec<u8>, value: &str) {
        u32b(out, value.len() as u32);
        out.extend(value.as_bytes());
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    fn attrs(out: &mut Vec<u8>, items: &[(&str, &str)]) {
        if items.is_empty() {
            out.extend([0; 8]);
            return;
        }
        u32b(out, 12);
        u32b(out, items.len() as u32);
        for (key, value) in items {
            name(out, key);
            u32b(out, 2);
            u32b(out, value.len() as u32);
            out.extend(value.as_bytes());
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
    }
    let wkt = SpatialRef::from_epsg(4326).unwrap().to_wkt().unwrap();
    let values = [
        vec![0., 1., 2.],
        vec![50., 51., 52.],
        vec![
            1. + longitude_offset,
            if irregular {
                2.00000001 + longitude_offset
            } else {
                2. + longitude_offset
            },
            3. + longitude_offset,
            4. + longitude_offset,
        ],
        (0..3)
            .flat_map(|t| {
                (0..3).flat_map(move |y| {
                    (0..4).map(move |x| 100. * (t + 1) as f64 + 10. * y as f64 + x as f64)
                })
            })
            .collect(),
        vec![0.],
    ];
    let variables = [
        (
            "time",
            vec![0],
            vec![
                ("units", "days since 2024-01-01"),
                ("standard_name", "time"),
            ],
        ),
        (
            "lat",
            vec![1],
            vec![
                ("units", "degrees_north"),
                ("standard_name", "latitude"),
                ("axis", "Y"),
            ],
        ),
        (
            "lon",
            vec![2],
            vec![
                ("units", "degrees_east"),
                ("standard_name", "longitude"),
                ("axis", "X"),
            ],
        ),
        (
            "temperature",
            vec![0, 1, 2],
            if with_crs {
                vec![("units", "K"), ("grid_mapping", "crs")]
            } else {
                vec![("units", "K")]
            },
        ),
        (
            "crs",
            vec![],
            vec![
                ("grid_mapping_name", "latitude_longitude"),
                ("spatial_ref", wkt.as_str()),
            ],
        ),
    ];
    let header = |starts: &[usize]| {
        let mut out = b"CDF\x01".to_vec();
        u32b(&mut out, 0);
        u32b(&mut out, 10);
        u32b(&mut out, 3);
        for (n, s) in [("time", 3), ("lat", 3), ("lon", 4)] {
            name(&mut out, n);
            u32b(&mut out, s);
        }
        attrs(&mut out, &[("Conventions", "CF-1.8")]);
        u32b(&mut out, 11);
        u32b(&mut out, 5);
        for (i, (n, dims, attributes)) in variables.iter().enumerate() {
            name(&mut out, n);
            u32b(&mut out, dims.len() as u32);
            for d in dims {
                u32b(&mut out, *d);
            }
            attrs(&mut out, attributes);
            u32b(&mut out, 6);
            u32b(&mut out, (values[i].len() * 8) as u32);
            u32b(&mut out, starts[i] as u32);
        }
        out
    };
    let mut offset = header(&[0; 5]).len();
    let starts: Vec<_> = values
        .iter()
        .map(|v| {
            let start = offset;
            offset += v.len() * 8;
            start
        })
        .collect();
    let mut data = header(&starts);
    for vector in values {
        for value in vector {
            data.extend(value.to_be_bytes());
        }
    }
    fs::write(path, data).unwrap();
}
#[test]
fn netcdf_native_time_slice_checks_actual_coordinate_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let regular = dir.path().join("dates.nc");
    let irregular = dir.path().join("irregular.nc");
    netcdf_fixture(&regular, false, true, 0.);
    netcdf_fixture(&irregular, true, true, 0.);
    let cancel = AtomicBool::new(false);
    let source = open_source(
        &spec(json!({"location":regular,"format":"netcdf","variable":"temperature","bands":[2,0]})),
        &cancel,
    )
    .unwrap();
    assert_eq!(
        source.metadata().grid.transform,
        [0.5, 1., 0., 52.5, 0., -1.]
    );
    let (window, _) = source
        .read_selected_window_cancellable(0, 0, 4, 3, &[0, 1], 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(&window.bands[0].values[..4], &[320., 321., 322., 323.]);
    assert_eq!(&window.bands[1].values[8..], &[100., 101., 102., 103.]);
    assert!(
        source.access_layout()["coordinate_validation"]["coordinate_bytes_read"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        open_source(
            &spec(json!({"location":irregular,"format":"netcdf","variable":"temperature"})),
            &cancel
        )
        .is_err()
    );
}
#[test]
fn netcdf_explicit_crs_and_longitude_translation_are_interpretation() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("unassigned.nc");
    let shifted = dir.path().join("longitude_360.nc");
    netcdf_fixture(&plain, false, false, 0.);
    netcdf_fixture(&shifted, false, true, 200.);
    let cancel = AtomicBool::new(false);
    assert!(
        open_source(
            &spec(json!({"location":plain,"format":"netcdf","variable":"temperature"})),
            &cancel
        )
        .is_err()
    );
    let assigned = open_source(
        &spec(
            json!({"location":plain,"format":"netcdf","variable":"temperature","crs":"EPSG:4326"}),
        ),
        &cancel,
    )
    .unwrap();
    assert_eq!(
        assigned.metadata().grid.transform,
        [0.5, 1., 0., 52.5, 0., -1.]
    );
    assert!(!std::path::PathBuf::from(format!("{}.aux.xml", plain.display())).exists());
    assert!(open_source(
        &spec(
            json!({"location":shifted,"format":"netcdf","variable":"temperature","crs":"EPSG:4326"})
        ),
        &cancel
    )
    .is_err());
    assert!(open_source(&spec(json!({"location":shifted,"format":"netcdf","variable":"temperature","crs":"EPSG:3857","longitude_shift":-360})),&cancel).is_err());
    let translated=open_source(&spec(json!({"location":shifted,"format":"netcdf","variable":"temperature","crs":"EPSG:4326","longitude_shift":-360})),&cancel).unwrap();
    assert_eq!(
        translated.metadata().grid.transform,
        [-159.5, 1., 0., 52.5, 0., -1.]
    );
    assert_eq!(translated.access_layout()["longitude_shift"], -360);
    let (window, _) = translated
        .read_selected_window_cancellable(0, 0, 4, 3, &[2], 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(&window.bands[0].values[..4], &[320., 321., 322., 323.]);
}

#[test]
fn portable_registration_relocates_but_rejects_mutation_and_mapping_change() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("copy.tif");
    fixture(&a);
    fs::copy(&a, &b).unwrap();
    let bytes = fs::read(&a).unwrap();
    let identity = json!({"sha256":format!("{:x}",Sha256::digest(&bytes)),"byte_length":bytes.len(),"policy":"verify"});
    let cancel = AtomicBool::new(false);
    let one = open_source(
        &spec(json!({"location":a,"bands":[2,1],"identity":identity})),
        &cancel,
    )
    .unwrap();
    let two = open_source(
        &spec(json!({"location":b,"bands":[2,1],"identity":identity})),
        &cancel,
    )
    .unwrap();
    assert_eq!(one.metadata().source_id, two.metadata().source_id);
    let (window, _) = two
        .read_selected_window_cancellable(1, 2, 2, 1, &[1, 0], 8 * 1024 * 1024, &cancel)
        .unwrap();
    assert_eq!(window.bands[0].values, vec![-827., -829.]);
    assert_eq!(window.bands[1].values, vec![416., 417.]);
    let index = dir.path().join("index");
    persistent::build_from_source(
        one.as_ref(),
        index.to_str().unwrap(),
        64,
        "band_major",
        "hierarchy",
        BoundarySource::Original,
        &cancel,
    )
    .unwrap();
    let mut reader = PersistedIndex::open(
        index.join("summary.rsi").to_str().unwrap(),
        None,
        1024 * 1024,
        &cancel,
    )
    .unwrap();
    assert_eq!(reader.header().version, 2);
    reader.verify_source(two.as_ref()).unwrap();
    let leaf = reader.read_leaf_summary(0, &cancel).unwrap();
    assert_eq!(leaf.len(), 2);
    assert_eq!(leaf[0].valid_count, 4096);
    let changed_mapping = open_source(
        &spec(json!({"location":b,"bands":[1,2],"identity":identity})),
        &cancel,
    )
    .unwrap();
    assert!(reader.verify_source(changed_mapping.as_ref()).is_err());
    fs::OpenOptions::new()
        .append(true)
        .open(&b)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(two.verify_immutable().is_err());
    assert!(open_source(&spec(json!({"location":b,"identity":identity})), &cancel).is_err());
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Server {
    fn start(bytes: Vec<u8>, mode: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/source.tif?signature=test-secret",
            listener.local_addr().unwrap()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let requests = Arc::new(std::sync::Mutex::new(vec![]));
        let records = requests.clone();
        let mode = mode.to_owned();
        let worker = thread::spawn(move || {
            while !stop_worker.load(Ordering::Relaxed) {
                let (mut c, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                };
                c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut data = vec![];
                let mut byte = [0u8; 1];
                while !data.ends_with(b"\r\n\r\n") && data.len() < 16384 {
                    if c.read_exact(&mut byte).is_err() {
                        break;
                    }
                    data.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&data).to_string();
                records.lock().unwrap().push(request.clone());
                let etag = if mode == "weak" {
                    "W/\"fixture\""
                } else {
                    "\"fixture\""
                };
                if request.starts_with("HEAD ") {
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                let range = request
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_owned)
                    })
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let (start, end) = (
                    start.parse::<usize>().unwrap(),
                    end.parse::<usize>().unwrap(),
                );
                if mode == "ignored" {
                    let _ = write!(
                        c,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    continue;
                }
                let returned = if mode == "changed" && records.lock().unwrap().len() > 1 {
                    "\"changed\""
                } else {
                    etag
                };
                let _ = write!(
                    c,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: {returned}\r\nConnection: close\r\n\r\n",
                    end - start + 1,
                    bytes.len()
                );
                let actual_end = if mode == "truncated" { start } else { end + 1 };
                let _ = c.write_all(&bytes[start..actual_end]);
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
            requests,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
#[test]
fn ordinary_remote_vsi_selected_bands_conditional_ranges_and_faults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.tif");
    fixture(&path);
    let bytes = fs::read(&path).unwrap();
    let cancel = AtomicBool::new(false);
    for mode in ["ok", "weak", "changed", "ignored", "truncated"] {
        let server = Server::start(bytes.clone(), mode);
        let config = spec(
            json!({"location":server.url,"bands":[2,1],"http":{"allow_http":true,"headers":{"Authorization":"Bearer header-secret"},"max_requests":512}}),
        );
        let source = open_source(&config, &cancel);
        if mode != "ok" {
            assert!(
                source.is_err(),
                "fault {mode} must reject during initial decode"
            );
            continue;
        }
        let source = source.unwrap();
        let (window, _) = source
            .read_selected_window_cancellable(1, 2, 2, 1, &[1, 0], 8 * 1024 * 1024, &cancel)
            .unwrap();
        assert_eq!(window.bands[0].values, vec![-827., -829.]);
        assert_eq!(window.bands[1].values, vec![416., 417.]);
        let diag = source.diagnostics();
        let text = diag.to_string();
        assert!(
            !text.contains("secret") && !text.contains("signature") && !text.contains("127.0.0.1")
        );
        assert!(diag["remote"]["ranges"].as_array().unwrap().len() > 0);
        for (i, request) in server.requests.lock().unwrap().iter().enumerate() {
            assert!(request.contains("authorization: Bearer header-secret"));
            if request.starts_with("GET ") && i > 0 {
                assert!(request.contains("if-match: \"fixture\""));
            }
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(
            source
                .read_selected_window_cancellable(0, 0, 1, 1, &[0], 8 * 1024 * 1024, &cancel)
                .is_err()
        );
        cancel.store(false, Ordering::Relaxed);
    }
}

#[test]
fn physical_chunk_cache_retains_one_footprint_and_releases_at_window_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("blocks512.tif");
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=512",
        "BLOCKYSIZE=512",
        "COMPRESS=DEFLATE",
        "INTERLEAVE=PIXEL",
    ]);
    let mut ds = driver
        .create_with_band_type_with_options::<f64, _>(&path, 512, 1024, 2, &opts)
        .unwrap();
    ds.set_geo_transform(&[0., 1., 0., 1024., 0., -1.]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for band in 1..=2 {
        let mut b = ds.rasterband(band).unwrap();
        b.set_no_data_value(Some(-9999.)).unwrap();
        b.set_scale(-0.25).unwrap();
        b.set_offset(3.).unwrap();
        let values = (0..512 * 1024)
            .map(|i| {
                if (i + band) % 23 == 0 {
                    -9999.
                } else {
                    (i % 733) as f64 + band as f64
                }
            })
            .collect();
        b.write((0, 0), (512, 1024), &mut Buffer::new((512, 1024), values))
            .unwrap();
    }
    ds.flush_cache().unwrap();
    drop(ds);
    let cancel = AtomicBool::new(false);
    let source = open_source(&spec(json!({"location":path})), &cancel).unwrap();
    let bound = source.read_buffer_bound(512, 512, &[1, 0]).unwrap();
    assert!(bound < 32 * 1024 * 1024);
    assert!(
        source
            .read_selected_window_cancellable(0, 0, 512, 512, &[1, 0], bound - 1, &cancel)
            .is_err()
    );
    let (raster, metrics) = source
        .read_selected_window_cancellable(0, 0, 512, 512, &[1, 0], bound, &cancel)
        .unwrap();
    assert_eq!(metrics.decoder_cache_flushes, 1);
    assert_eq!(metrics.decoder_cache_reused_chunks, 3);
    for (out, band) in raster.bands.iter().zip([2, 1]) {
        for i in 0..512 * 512 {
            let valid = (i + band) % 23 != 0;
            assert_eq!(out.valid[i], valid);
            assert_eq!(
                out.values[i],
                if valid {
                    ((i % 733) as f64 + band as f64) * -0.25 + 3.
                } else {
                    0.
                }
            );
        }
    }
    let (_, again) = source
        .read_selected_window_cancellable(0, 0, 512, 512, &[1, 0], bound, &cancel)
        .unwrap();
    assert_eq!(again.decoder_cache_flushes, 1);
    assert_eq!(source.diagnostics()["decoder_cache_flushes"], 2);
}
