//! Independent regression cases for hierarchy/persistence/session review.
use raster_engine::{
    aggregate::Options,
    model::Grid,
    persistent::{Header, QueryOptions},
};
use serde_json::json;
use std::{fs, sync::atomic::AtomicBool};

fn page(mut bytes: Vec<u8>) -> Vec<u8> {
    let hash = blake3::hash(&bytes);
    bytes.extend_from_slice(hash.as_bytes());
    bytes
}
fn header(h: &Header) -> Vec<u8> {
    let encoded = serde_json::to_vec(h).unwrap();
    let mut b = vec![0; 65536 - 32];
    b[..8].copy_from_slice(b"RSTRLAB1");
    b[8..16].copy_from_slice(&(encoded.len() as u64).to_le_bytes());
    b[16..16 + encoded.len()].copy_from_slice(&encoded);
    page(b)
}

#[test]
fn architecture_cumulative_review_rejects_checksumming_an_impossible_empty_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Header {
        source_identity: None,
        version: 1,
        boundary_source: Default::default(),
        kind: "summaries".into(),
        numerical_contract: "native_grid_planar_v1".into(),
        algorithm_version: "flat_tiles_v1".into(),
        grid: Grid {
            width: 32,
            height: 32,
            transform: [0., 1., 0., 0., 0., -1.],
            crs: "LOCAL".into(),
        },
        source_id: "review-fixture".into(),
        source_metadata: json!({}),
        build_id: "0".repeat(64),
        units: vec![None],
        tile_edge: 16,
        band_group: 1,
        capabilities: ["sum", "support", "count", "min", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        byte_order: "little".into(),
        scalar: "normalized_f64_u8_valid".into(),
    };
    let ip = dir.path().join("summary.rsi");
    let rp = dir.path().join("pixels.rsr");
    for (sum, count, min, max, valid_control) in [
        (1., 0_u64, 0., 0., false), // No valid cell cannot sum to one.
        (1., 1, 0., 0., false),     // Single zero-valued cell cannot sum to one.
        (0., 257, 0., 0., false),   // More valid cells than tile area.
        (1., 1, 2., 1., false),     // Reversed extrema.
        (512., 256, 2., 2., true),  // Genuine valid control, raw values match.
    ] {
        h.kind = "summaries".into();
        let mut summary = header(&h);
        let mut state = vec![0; 40];
        state[..8].copy_from_slice(&f64::to_le_bytes(sum));
        state[16..24].copy_from_slice(&count.to_le_bytes());
        state[24..32].copy_from_slice(&f64::to_le_bytes(min));
        state[32..40].copy_from_slice(&f64::to_le_bytes(max));
        for _ in 0..4 {
            summary.extend(page(state.clone()));
        }
        h.kind = "raw".into();
        let mut raw = header(&h);
        let mut pixels = vec![0; 16 * 16 * 9];
        if valid_control {
            for i in 0..256 {
                pixels[i * 9..i * 9 + 8].copy_from_slice(&2_f64.to_le_bytes());
                pixels[i * 9 + 8] = 1;
            }
        }
        for _ in 0..4 {
            raw.extend(page(pixels.clone()));
        }
        fs::write(&ip, summary).unwrap();
        fs::write(&rp, raw).unwrap();
        let result = raster_engine::persistent::query(
            QueryOptions {
                joint_planner: None,
                index_path: ip.to_str().unwrap(),
                raw_path: rp.to_str().unwrap(),
                source_path: None,
                expected_build_id: Some(&h.build_id),
                read_memory_bytes: 32 * 1024 * 1024,
                summary_page_bytes: Some(0),
                coalesce_raw: true,
                order_summaries: false,
                hierarchy: None,
                forbidden_raw_tiles: &[],
                force_direct: false,
            },
            &json!({"type":"Polygon","coordinates":[[[-1.,1.],[33.,1.],[33.,-33.],[-1.,-33.],[-1.,1.]]]}),
            "LOCAL",
            &Options::default(),
            &AtomicBool::new(false),
        );
        if valid_control {
            let value = result.unwrap();
            assert_eq!(value["bands"][0]["fractional_sum"], 2048.);
            assert_eq!(value["bands"][0]["covered_cell_equivalents"], 1024.);
        } else {
            assert!(
                result.is_err(),
                "Impossible state sum={sum} count={count} min={min} max={max} was accepted: {}",
                result.unwrap()
            );
        }
    }
}

#[test]
fn architecture_cumulative_review_huge_read_budget_is_an_error_not_a_panic() {
    let mut session = raster_engine::session::Session::default();
    let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.call(json!({"op":"measure_file","index":"/does-not-exist","read_memory_bytes":u64::MAX,"geometry":{"type":"Polygon","coordinates":[]},"crs":"LOCAL"}),&AtomicBool::new(false))
    }));
    assert!(
        call.is_ok(),
        "Untrusted read_memory_bytes overflowed before range validation"
    );
    assert!(call.unwrap().is_err());
}
