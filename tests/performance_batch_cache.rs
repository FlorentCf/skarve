//! Shared decoded data, strict numeric presentation and identity boundaries.
use raster_engine::{
    batch::{Job, JobSpec, ResidentSource},
    model::{Band, Grid, Raster},
    tile_cache::{TileCache, source_key},
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn raster(version: &str) -> Raster {
    Raster {
        grid: Grid {
            width: 64,
            height: 2,
            transform: [0., 1., 0., 2., 0., -1.],
            crs: "LOCAL".into(),
        },
        bands: (0..2)
            .map(|b| Band {
                values: (0..128)
                    .map(|i| {
                        if i % 11 == 0 {
                            f64::NAN
                        } else {
                            (i as f64 - 63.) * if b == 0 { 0.25 } else { -2. }
                        }
                    })
                    .collect(),
                valid: (0..128).map(|i| i % 11 != 0).collect(),
                unit: Some("constructed units".into()),
            })
            .collect(),
        source_id: version.into(),
    }
}
fn definition(mode: &str) -> JobSpec {
    serde_json::from_value(json!({
        "crs":"LOCAL", "tile_edge":32, "output_mode":mode, "geometry_layout":"compact",
        "options":{"statistics":["sum","support","mean","min","max","count"]},
        "slices":[{"id":"a","source":"r","bands":[0,1]},{"id":"b","source":"r","bands":[1,0]}],
        "zones":[
            {"id":"full","version":"1","geometry":{"type":"Polygon","coordinates":[[[0,0],[64,0],[64,2],[0,2],[0,0]]]}},
            {"id":"thin","version":"1","geometry":{"type":"Polygon","coordinates":[[[1.25,0.1],[63.75,0.1],[63.75,0.10000001],[1.25,0.10000001],[1.25,0.1]]]}},
            {"id":"outside","version":"1","geometry":{"type":"Polygon","coordinates":[[[80,0],[90,0],[90,2],[80,2],[80,0]]]}}
        ]
    })).unwrap()
}
fn run(job: &mut Job, raster: &Raster, cache: &mut TileCache) -> Vec<Value> {
    let mut rows = vec![];
    loop {
        let page = job
            .next_with_cache(
                4096,
                false,
                |_| Ok(Box::new(ResidentSource::new(raster))),
                &AtomicBool::new(false),
                cache,
            )
            .unwrap();
        rows.extend(page["rows"].as_array().unwrap().iter().cloned());
        if page["complete"] == true {
            return rows;
        }
    }
}
#[test]
fn batch_cache_reuses_values_across_band_order_and_jobs_but_not_source_versions() {
    let r = raster("version-1");
    let mut cache = TileCache::default();
    cache.set_limit(64 << 10).unwrap();
    let mut first = Job::new(definition("full"), None, 32768, 1024 << 20).unwrap();
    let first_rows = run(&mut first, &r, &mut cache);
    assert_eq!(first.metrics.windows_read, 2);
    assert_eq!(first.metrics.decoded_cache_hits, 2);
    assert_eq!(first.metrics.geometry_compilations, 3);
    assert_eq!(first.metrics.geometry_cache_hits, 3);
    assert_eq!(first.metrics.decoded_cache_hit_payload_bytes, 128 * 2 * 9);
    let mut second = Job::new(definition("full"), None, 32768, 1024 << 20).unwrap();
    assert_eq!(first_rows, run(&mut second, &r, &mut cache));
    assert_eq!(second.metrics.windows_read, 0);
    assert_eq!(second.metrics.decoded_cache_hits, 4);
    assert_eq!(second.metrics.decoded_value_bytes, 0);
    let changed = raster("version-2");
    let mut third = Job::new(definition("full"), None, 32768, 1024 << 20).unwrap();
    run(&mut third, &changed, &mut cache);
    assert_eq!(third.metrics.windows_read, 2);
    assert!(cache.bytes() <= cache.limit());
}
#[test]
fn numeric_direct_finish_preserves_all_six_values_for_signed_masked_thin_empty_support() {
    let r = raster("numeric-identity");
    let mut full = Job::new(definition("full"), None, 32768, 1024 << 20).unwrap();
    let mut numeric = Job::new(definition("numeric"), None, 32768, 1024 << 20).unwrap();
    let a = run(&mut full, &r, &mut TileCache::default());
    let b = run(&mut numeric, &r, &mut TileCache::default());
    for (full, numeric) in a.iter().zip(&b) {
        assert_eq!(full["result_id"], numeric["result_id"]);
        for (a, b) in full["bands"]
            .as_array()
            .unwrap()
            .iter()
            .zip(numeric["bands"].as_array().unwrap())
        {
            for name in [
                "band",
                "fractional_sum",
                "covered_cell_equivalents",
                "valid_cell_count",
                "coverage_weighted_mean",
                "min",
                "max",
            ] {
                assert_eq!(a[name], b[name], "{name}");
            }
        }
    }
    assert_eq!(numeric.metrics.numeric_results_direct, 12);
    assert_eq!(numeric.metrics.reducer_identity_builds, 2);
    assert_eq!(numeric.metrics.accumulator_states, 12);
}
#[test]
fn decoded_lru_is_byte_bounded_accounts_evictions_and_rejects_oversize() {
    let raster = Arc::new(raster("lru"));
    let mut cache = TileCache::default();
    cache.set_limit(raster.bytes() + 512).unwrap();
    cache.insert("one".into(), Arc::clone(&raster));
    assert!(cache.get("one").is_some());
    cache.insert("two".into(), Arc::clone(&raster));
    assert!(cache.get("one").is_none());
    assert!(cache.get("two").is_some());
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.stats().admissions, 2);
    assert_eq!(cache.stats().hit_payload_bytes, 2 * 128 * 2 * 9);
    cache.set_limit(1).unwrap();
    cache.insert("too-large".into(), raster);
    assert_eq!(cache.stats().admission_rejections, 1);
    assert!(cache.is_empty());
}
#[test]
fn decoded_identity_separates_grid_band_units_and_declared_source_generation() {
    let a = raster("same-id");
    let original = source_key(&ResidentSource::new(&a)).unwrap();
    let mut b = raster("same-id");
    b.grid.transform[0] = 1.;
    assert_ne!(original, source_key(&ResidentSource::new(&b)).unwrap());
    b = raster("same-id");
    b.bands[0].unit = Some("another interpretation".into());
    assert_ne!(original, source_key(&ResidentSource::new(&b)).unwrap());
    assert_ne!(
        original,
        source_key(&ResidentSource::new(&raster("different-id"))).unwrap()
    );
}
