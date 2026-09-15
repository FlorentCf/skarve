use raster_engine::{backend, session::Session};
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[test]
fn installed_availability_and_strict_owned_array_default() {
    let mut session = Session::default();
    let cancel = AtomicBool::new(false);
    let capabilities = session.call(json!({"op":"backends"}), &cancel).unwrap();
    assert_eq!(capabilities["exactextract"], cfg!(feature = "exactextract"));
    session
        .call(
            json!({"op":"open","id":"owned","raster":{
        "grid":{"width":3,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},
        "bands":[{"values":[9007199254740992.0,1,-9007199254740992.0]}]}}),
            &cancel,
        )
        .unwrap();
    let mut query = json!({"op":"measure","source":"owned","crs":"LOCAL",
        "geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,1],[0,1],[0,0]]]},
        "statistics":["sum","support","mean","min","max"],"backend":"auto"});
    let native = session.call(query.clone(), &cancel).unwrap();
    assert_eq!(native["bands"][0]["fractional_sum"], 1.0);
    assert_eq!(native["provenance"]["selected_backend"], "native");
    query["backend"] = json!("exactextract");
    if cfg!(feature = "exactextract") {
        let upstream = session.call(query.clone(), &cancel).unwrap();
        assert_eq!(
            upstream["provenance"]["numerical_policy"],
            backend::EXACTEXTRACT_POLICY
        );
        assert_eq!(upstream["bands"][0]["fractional_sum"], 0.0);
    } else {
        assert!(
            session
                .call(query.clone(), &cancel)
                .unwrap_err()
                .to_string()
                .contains("not installed")
        );
    }
    query["numerical_policy"] = json!(backend::NATIVE_POLICY);
    assert!(session.call(query, &cancel).is_err());
}

#[cfg(feature = "exactextract")]
mod enabled {
    use super::*;
    use raster_engine::{
        aggregate::Options,
        batch::ResidentSource,
        exactextract::{self, Input},
        model::{Raster, RasterInput},
        source::{RasterMetadata, ReadMetrics, WindowSource},
        tile_cache::TileCache,
    };
    use std::{cell::Cell, sync::atomic::Ordering};
    struct FaultSource<'a> {
        inner: ResidentSource<'a>,
        fault: Cell<u8>,
        reads: Cell<usize>,
    }
    impl WindowSource for FaultSource<'_> {
        fn metadata(&self) -> &RasterMetadata {
            self.inner.metadata()
        }
        fn verify_immutable(&self) -> anyhow::Result<()> {
            anyhow::ensure!(
                self.fault.get() != 3 || self.reads.get() == 0,
                "source changed after callback"
            );
            self.inner.verify_immutable()
        }
        fn read_selected_window_cancellable(
            &self,
            x: usize,
            y: usize,
            w: usize,
            h: usize,
            b: &[usize],
            m: usize,
            c: &AtomicBool,
        ) -> anyhow::Result<(Raster, ReadMetrics)> {
            self.reads.set(self.reads.get() + 1);
            anyhow::ensure!(self.fault.get() != 1, "injected source read failure");
            let result = self
                .inner
                .read_selected_window_cancellable(x, y, w, h, b, m, c)?;
            if self.fault.get() == 2 {
                c.store(true, Ordering::Relaxed);
            }
            Ok(result)
        }
    }
    fn raster(values: serde_json::Value) -> Raster {
        serde_json::from_value::<RasterInput>(json!({"grid":{"width":2,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":[{"values":values}]})).unwrap().decode().unwrap()
    }
    fn zone() -> serde_json::Value {
        json!({"type":"Polygon","coordinates":[[[0,0],[2,0],[2,1],[0,1],[0,0]]]})
    }
    #[test]
    fn source_failure_cancellation_mutation_and_recovery_discard_whole_output() {
        let data = raster(json!([3, 7]));
        let source = FaultSource {
            inner: ResidentSource::new(&data),
            fault: Cell::new(0),
            reads: Cell::new(0),
        };
        let input = [Input {
            source: &source,
            bands: vec![0],
        }];
        let cancel = AtomicBool::new(false);
        for strategy in ["feature-sequential", "raster-sequential"] {
            let limits = backend::EeOptions {
                strategy: strategy.into(),
                ..Default::default()
            };
            for fault in [1, 2, 3] {
                source.fault.set(fault);
                source.reads.set(0);
                cancel.store(false, Ordering::Relaxed);
                let failed = exactextract::execute(
                    &input,
                    &[zone()],
                    "LOCAL",
                    &Options::default(),
                    &limits,
                    &cancel,
                    &mut TileCache::default(),
                    512 << 20,
                );
                assert!(failed.is_err());
                assert!(source.reads.get() > 0);
                source.fault.set(0);
                cancel.store(false, Ordering::Relaxed);
                let recovered = exactextract::execute(
                    &input,
                    &[zone()],
                    "LOCAL",
                    &Options::default(),
                    &limits,
                    &cancel,
                    &mut TileCache::default(),
                    512 << 20,
                )
                .unwrap();
                assert_eq!(recovered.values[0], 10.0);
                assert_eq!(recovered.metrics["pixel_copy_bytes_in_cpp"], 0);
            }
        }
    }
    #[test]
    fn selected_extrema_do_not_compute_an_unrequested_overflowing_sum() {
        let data = raster(json!([f64::MAX, f64::MAX]));
        let source = ResidentSource::new(&data);
        let input = [Input {
            source: &source,
            bands: vec![0],
        }];
        let cancel = AtomicBool::new(false);
        for strategy in ["feature-sequential", "raster-sequential"] {
            let limits = backend::EeOptions {
                strategy: strategy.into(),
                ..Default::default()
            };
            for selected in [
                vec!["min"],
                vec!["max"],
                vec!["min", "max"],
                vec!["support"],
            ] {
                let options = Options {
                    statistics: Some(selected.iter().map(|s| s.to_string()).collect()),
                    ..Default::default()
                };
                let output = exactextract::execute(
                    &input,
                    &[
                        zone(),
                        json!({"type":"Polygon","coordinates":[[[3,0],[4,0],[4,1],[3,1],[3,0]]]}),
                    ],
                    "LOCAL",
                    &options,
                    &limits,
                    &cancel,
                    &mut TileCache::default(),
                    512 << 20,
                )
                .unwrap();
                assert_eq!(output.defined[0], 0, "unrequested sum was evaluated");
                assert_eq!(output.values[1], 2.0, "internal status support");
                assert_eq!(output.defined[2], 0, "unrequested mean was evaluated");
                for (slot, field) in [(3, "min"), (4, "max")] {
                    assert_eq!(output.defined[slot], u8::from(selected.contains(&field)));
                    if selected.contains(&field) {
                        assert_eq!(output.values[slot], f64::MAX);
                    }
                }
                assert_eq!(
                    output.bands(0, 0, options.statistics.as_ref().unwrap())[0]["status"],
                    "ok"
                );
                assert_eq!(
                    output.bands(1, 0, options.statistics.as_ref().unwrap())[0]["status"],
                    "no_valid_data"
                );
            }
        }
    }
    #[test]
    fn preadmission_and_nonfinite_output_reject_without_partial_answers() {
        let data = raster(json!([f64::MAX, f64::MAX]));
        let source = FaultSource {
            inner: ResidentSource::new(&data),
            fault: Cell::new(0),
            reads: Cell::new(0),
        };
        let input = [Input {
            source: &source,
            bands: vec![0],
        }];
        let cancel = AtomicBool::new(false);
        let limits = backend::EeOptions::default();
        assert!(
            exactextract::execute(
                &input,
                &[zone()],
                "LOCAL",
                &Options::default(),
                &limits,
                &cancel,
                &mut TileCache::default(),
                1 << 20
            )
            .is_err()
        );
        assert_eq!(source.reads.get(), 0);
        assert!(
            exactextract::execute(
                &input,
                &[zone()],
                "LOCAL",
                &Options::default(),
                &limits,
                &cancel,
                &mut TileCache::default(),
                512 << 20
            )
            .is_err()
        );
    }
}
