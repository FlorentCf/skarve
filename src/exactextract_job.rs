//! Finite all-feature/all-band upstream jobs with bounded typed staging and pages.
use crate::{
    backend::Decision,
    batch::{JobSpec, OutputMode},
    exactextract::{Input, Output},
    model::check_cancel,
    source::WindowSource,
    tile_cache::TileCache,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

pub struct Job {
    spec: JobSpec,
    decision: Decision,
    fingerprint: String,
    output: Option<Output>,
    cursor: usize,
    failed: bool,
    spec_bytes: usize,
    geometry_reserve: usize,
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::{
        backend::{Backend, EeOptions},
        model::{Grid, Raster},
        source::{BandMetadata, RasterMetadata, ReadMetrics},
    };
    use std::{cell::Cell, rc::Rc};

    fn spec() -> JobSpec {
        serde_json::from_value(json!({
            "zones":[{"id":"zone","version":"1","geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}}],
            "slices":[{"id":"slice","source":"registered","bands":[0]}],
            "crs":"EPSG:3857","options":{"statistics":["sum","support","mean","min","max"]}
        })).unwrap()
    }
    fn decision() -> Decision {
        Decision {
            selected: Backend::Exactextract,
            provenance: json!({"numerical_policy":crate::backend::EXACTEXTRACT_POLICY}),
            options: EeOptions::default(),
        }
    }
    fn job(spec: JobSpec) -> Result<Job> {
        Job::new(spec, decision(), 1024, 1 << 30, false)
    }
    #[cfg(feature = "exactextract")]
    #[test]
    fn rasterio_job_fingerprint_and_provenance_are_distinct() {
        let make = |policy| {
            let decision = crate::backend::resolve(
                &json!({"backend":"exactextract","numerical_policy":policy}),
                false,
            )
            .unwrap();
            let limits = effective_limits(&spec(), &decision).unwrap();
            assert_eq!(
                limits.rasterio_compatible,
                policy == crate::backend::EXACTEXTRACT_RASTERIO_POLICY
            );
            Job::new(spec(), decision, 1024, 1 << 30, false).unwrap()
        };
        let legacy = make(crate::backend::EXACTEXTRACT_POLICY);
        let compatible = make(crate::backend::EXACTEXTRACT_RASTERIO_POLICY);
        assert_ne!(
            legacy.info()["fingerprint"],
            compatible.info()["fingerprint"]
        );
        assert_eq!(
            compatible.info()["provenance"]["source_interpretation"],
            "rasterio_unscaled_raw_v030"
        );
        assert_eq!(
            make(crate::backend::EXACTEXTRACT_POLICY).info()["fingerprint"],
            legacy.info()["fingerprint"]
        );
    }
    struct MetadataOnlySource {
        metadata: RasterMetadata,
        retained: usize,
        reads: Rc<Cell<usize>>,
    }
    impl MetadataOnlySource {
        fn new(retained: usize, reads: Rc<Cell<usize>>) -> Self {
            Self {
                metadata: RasterMetadata {
                    grid: Grid {
                        width: 2,
                        height: 2,
                        transform: [0., 1., 0., 2., 0., -1.],
                        crs: "EPSG:3857".into(),
                    },
                    bands: vec![BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: None,
                        block_size: (2, 2),
                    }],
                    source_id: "synthetic".into(),
                },
                retained,
                reads,
            }
        }
    }
    impl WindowSource for MetadataOnlySource {
        fn metadata(&self) -> &RasterMetadata {
            &self.metadata
        }
        fn verify_immutable(&self) -> Result<()> {
            Ok(())
        }
        fn retained_memory_bound(&self) -> usize {
            self.retained
        }
        fn read_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: usize,
            _: &[usize],
            _: usize,
            _: &AtomicBool,
        ) -> Result<(Raster, ReadMetrics)> {
            self.reads.set(self.reads.get() + 1);
            anyhow::bail!("unexpected pixel read during pre-admission test")
        }
    }
    #[test]
    fn rejects_ignored_native_execution_controls() {
        let mut s = spec();
        s.schedule = crate::batch::Schedule::Feature;
        assert!(job(s).is_err());
        let mut s = spec();
        s.geometry_layout = crate::batch::GeometryLayout::Compact;
        assert!(job(s).is_err());
        let mut s = spec();
        s.window_policy = crate::batch::WindowPolicy::SourceLayout;
        assert!(job(s).is_err());
        let mut s = spec();
        s.tile_edge = 128;
        assert!(job(s).is_err());
    }
    #[test]
    fn geometry_and_effective_output_limits_fail_before_open() {
        let mut s = spec();
        s.budget.geometry_bytes = 1500;
        assert!(job(s).is_err());
        let mut s = spec();
        s.budget.output_bytes = 8191;
        assert!(job(s).is_err());
        let mut s = spec();
        s.budget.tile_bytes = 4095;
        assert!(job(s).is_err());
    }
    #[test]
    fn contribution_budget_rejects_before_pixels_and_marks_job_failed() {
        let mut s = spec();
        s.budget.max_contributions = 3;
        let mut job = job(s).unwrap();
        let reads = Rc::new(Cell::new(0));
        let result = job.next(
            1,
            |_| Ok(Box::new(MetadataOnlySource::new(0, Rc::clone(&reads)))),
            &AtomicBool::new(false),
            &mut TileCache::default(),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("max_contributions")
        );
        assert_eq!(reads.get(), 0);
        assert!(job.failed);
        assert_eq!(job.cursor, 0);
        assert!(job.output.is_none());
    }
    #[test]
    fn oversized_reader_fails_before_next_reader_or_pixels() {
        let mut s = spec();
        let mut second = s.slices[0].clone();
        second.id = "second".into();
        s.slices.push(second);
        let mut job = job(s).unwrap();
        let opens = Cell::new(0);
        let reads = Rc::new(Cell::new(0));
        let result = job.next(
            1,
            |_| {
                opens.set(opens.get() + 1);
                Ok(Box::new(MetadataOnlySource::new(
                    crate::source::NATIVE_SOURCE_RETAINED_BYTES + 1,
                    Rc::clone(&reads),
                )))
            },
            &AtomicBool::new(false),
            &mut TileCache::default(),
        );
        assert!(result.unwrap_err().to_string().contains("retained-memory"));
        assert_eq!(opens.get(), 1);
        assert_eq!(reads.get(), 0);
        assert!(job.failed);
    }
    #[test]
    fn cancellation_drops_completed_staging_and_prevents_reuse() {
        let mut job = job(spec()).unwrap();
        job.output = Some(Output {
            descriptors: vec![],
            values: vec![1.; 5],
            defined: vec![1; 5],
            band_count: 1,
            metrics: json!({}),
        });
        let result = job.next(
            1,
            |_| anyhow::bail!("must not open"),
            &AtomicBool::new(true),
            &mut TileCache::default(),
        );
        assert!(result.is_err());
        assert!(job.failed);
        assert!(job.output.is_none());
    }
    #[test]
    fn contribution_bound_counts_bands_and_zones_and_checks_overflow() {
        let mut source = MetadataOnlySource::new(0, Rc::new(Cell::new(0)));
        let inputs = [Input {
            source: &source,
            bands: vec![0, 1],
        }];
        assert_eq!(contribution_bound(&inputs, 3, 24).unwrap(), 24);
        assert!(contribution_bound(&inputs, 3, 23).is_err());
        source.metadata.grid.width = usize::MAX;
        assert!(
            contribution_bound(
                &[Input {
                    source: &source,
                    bands: vec![0]
                }],
                2,
                usize::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn analytical_fingerprint_excludes_transport_secrets_but_pins_content() {
        let mut first = spec();
        first.slices[0].source = None;
        first.slices[0].spec = Some(
            serde_json::from_value(json!({
                "location":"https://example.invalid/a?token=first",
                "identity":{"sha256":"11".repeat(32),"byte_length":100,"policy":"verify"},
                "http":{"headers":{"Authorization":"Bearer synthetic-first"}}
            }))
            .unwrap(),
        );
        let mut second = first.clone();
        let source = second.slices[0].spec.as_mut().unwrap();
        source.location = "https://example.invalid/b?token=second".into();
        source
            .http
            .headers
            .insert("Authorization".into(), "Bearer synthetic-second".into());
        let expected = job(first).unwrap().fingerprint;
        assert_eq!(expected, job(second.clone()).unwrap().fingerprint);
        second.slices[0]
            .spec
            .as_mut()
            .unwrap()
            .identity
            .as_mut()
            .unwrap()
            .sha256 = "22".repeat(32);
        assert_ne!(expected, job(second).unwrap().fingerprint);
    }
}

// Bound JSON nodes and conversion staging structurally; short coordinate text
// does not bound the allocation occupied by a retained serde_json::Value tree.
fn geometry_bound(value: &Value, depth: usize) -> Result<usize> {
    ensure!(
        depth <= 32,
        "exactextract geometry nesting exceeds admission limit"
    );
    match value {
        Value::Array(values) => values.iter().try_fold(128_usize, |bytes, value| {
            bytes
                .checked_add(geometry_bound(value, depth + 1)?)
                .ok_or_else(|| anyhow::anyhow!("exactextract geometry allocation bound overflow"))
        }),
        Value::Object(values) => values.iter().try_fold(128_usize, |bytes, (key, value)| {
            let child = geometry_bound(value, depth + 1)?;
            bytes
                .checked_add(key.len())
                .and_then(|n| n.checked_add(128))
                .and_then(|n| n.checked_add(child))
                .ok_or_else(|| anyhow::anyhow!("exactextract geometry allocation bound overflow"))
        }),
        Value::String(value) => 128_usize
            .checked_add(value.len())
            .ok_or_else(|| anyhow::anyhow!("exactextract geometry allocation bound overflow")),
        _ => Ok(128),
    }
}

fn effective_limits(spec: &JobSpec, decision: &Decision) -> Result<crate::backend::EeOptions> {
    let mut limits = decision.options.clone();
    limits.window_bytes = limits.window_bytes.min(spec.budget.tile_bytes);
    limits.output_bytes = limits.output_bytes.min(spec.budget.output_bytes / 2);
    limits.max_windows = limits.max_windows.min(spec.budget.max_windows);
    limits.decoded_bytes = limits.decoded_bytes.min(spec.budget.decoded_bytes);
    limits.validate()?;
    Ok(limits)
}

fn contribution_bound(inputs: &[Input<'_>], zones: usize, maximum: usize) -> Result<usize> {
    inputs.iter().try_fold(0_usize, |total, input| {
        let grid = &input.source.metadata().grid;
        let upper = grid.width.checked_mul(grid.height)
            .and_then(|n| n.checked_mul(input.bands.len()))
            .and_then(|n| n.checked_mul(zones))
            .and_then(|n| n.checked_add(total))
            .ok_or_else(|| anyhow::anyhow!("exactextract contribution preflight overflow"))?;
        ensure!(upper <= maximum, "exactextract conservative full-grid polygon-band contribution bound exceeds max_contributions");
        Ok(upper)
    })
}

fn row_bound(spec: &JobSpec, output: &Output, zone: usize, slice: usize) -> usize {
    let d = &output.descriptors[slice];
    let z = &spec.zones[zone];
    let s = &spec.slices[slice];
    let metadata =
        z.id.len()
            .saturating_add(z.version.len())
            .saturating_add(s.id.len())
            .saturating_add(d.source_id.len())
            .saturating_add(s.time.as_ref().map_or(0, String::len))
            .saturating_add(s.variable.as_ref().map_or(0, String::len));
    // Includes escaped strings, scalar JSON nodes/containers and serialization
    // staging. It deliberately overcharges numeric-only rows too.
    d.units.iter().fold(
        8192_usize.saturating_add(metadata.saturating_mul(8)),
        |total, unit| {
            total
                .saturating_add(2048)
                .saturating_add(unit.as_ref().map_or(0, String::len).saturating_mul(8))
        },
    )
}

fn page_envelope_bound(spec: &JobSpec, output: &Output, provenance: &Value) -> Result<usize> {
    let fields = geometry_bound(provenance, 0)?
        .saturating_mul(3)
        .saturating_add(geometry_bound(&output.metrics, 0)?)
        .saturating_add(8192);
    Ok(spec
        .slices
        .iter()
        .zip(&output.descriptors)
        .fold(fields, |total, (s, d)| {
            total
                .saturating_add(8192)
                .saturating_add(d.bands.len().saturating_mul(256))
                .saturating_add(
                    s.id.len()
                        .saturating_add(d.source_id.len())
                        .saturating_add(s.time.as_ref().map_or(0, String::len))
                        .saturating_add(s.variable.as_ref().map_or(0, String::len))
                        .saturating_mul(8),
                )
        }))
}
impl Job {
    pub fn new(
        spec: JobSpec,
        decision: Decision,
        spec_bytes: usize,
        available: usize,
        checkpoint: bool,
    ) -> Result<Self> {
        ensure!(
            !checkpoint,
            "exactextract jobs do not support resume checkpoints"
        );
        ensure!(
            (1..=512).contains(&spec.zones.len()) && (1..=32).contains(&spec.slices.len()),
            "exactextract finite batch allows 1..512 zones and 1..32 slices"
        );
        ensure!(
            spec.expression.is_none() && spec.mask.is_none(),
            "exactextract batch does not support expressions or derived masks"
        );
        ensure!(
            spec.slices
                .iter()
                .all(|s| s.index.is_none() && s.expected_build_id.is_none()),
            "exactextract batch cannot consume native prepared indexes"
        );
        crate::exactextract::statistics(&spec.options)?;
        ensure!(
            spec.schedule == crate::batch::Schedule::Tile
                && spec.geometry_layout == crate::batch::GeometryLayout::Auto
                && spec.window_policy == crate::batch::WindowPolicy::Fixed
                && spec.tile_edge == 256,
            "native schedule, geometry layout, window policy and tile edge are unavailable for exactextract jobs"
        );
        let limits = effective_limits(&spec, &decision)?;
        let geometry_reserve = spec.zones.iter().try_fold(0_usize, |total, zone| {
            total
                .checked_add(geometry_bound(&zone.geometry, 0)?)
                .ok_or_else(|| anyhow::anyhow!("exactextract geometry allocation bound overflow"))
        })?;
        ensure!(
            geometry_reserve <= spec.budget.geometry_bytes,
            "exactextract geometry staging exceeds geometry_bytes before source open"
        );
        let mut validation = spec.clone();
        validation.output_mode = OutputMode::Full;
        // Reuse existing bounded job/control/source/ID admission without replacing
        // the native implementation or pretending upstream computes its count.
        let validated_job = crate::batch::Job::new(validation, None, spec_bytes, available)?;
        // Match the native analytical identity boundary: credentials and source
        // transport locations never participate in the portable fingerprint.
        // Upstream strategy/chunking can affect reduction order, so include them.
        let fingerprint = blake3::hash(&serde_json::to_vec(&json!({
            "job":validated_job.info()["fingerprint"],
            "backend":"exactextract", "numerical_policy":decision.provenance["numerical_policy"],
            "source_interpretation":decision.provenance["source_interpretation"],
            "strategy":limits.strategy, "max_cells_in_memory":limits.max_cells_in_memory
        }))?)
        .to_hex()
        .to_string();
        drop(validated_job);
        ensure!(
            spec_bytes <= spec.budget.geometry_bytes,
            "exactextract job control exceeds geometry budget"
        );
        let reader_admission = spec
            .slices
            .len()
            .checked_mul(crate::source::NATIVE_SOURCE_RETAINED_BYTES)
            .ok_or_else(|| anyhow::anyhow!("reader reservation overflow"))?;
        ensure!(
            reader_admission
                .saturating_add(limits.window_bytes)
                .saturating_add(spec.budget.output_bytes)
                .saturating_add(128 << 20)
                .saturating_add(spec_bytes)
                .saturating_add(geometry_reserve)
                <= spec.budget.working_bytes,
            "exactextract all-reader admission exceeds working_bytes before source open"
        );
        Ok(Self {
            spec,
            decision,
            fingerprint,
            output: None,
            cursor: 0,
            failed: false,
            spec_bytes,
            geometry_reserve,
        })
    }
    pub fn reservation(&self) -> usize {
        self.spec.budget.working_bytes
    }
    pub fn info(&self) -> Value {
        json!({"fingerprint":self.fingerprint,"total_rows":self.spec.zones.len()*self.spec.slices.len(),"next_row":self.cursor,"budget":self.spec.budget,"provenance":self.decision.provenance,"checkpoint_supported":false,"checkpoint":null,"failed":self.failed,"execution":"one bounded upstream all-feature/all-band call, typed complete-job staging, paged presentation"})
    }
    pub fn next<'a, F>(
        &mut self,
        max_rows: usize,
        mut open: F,
        cancel: &AtomicBool,
        cache: &mut TileCache,
    ) -> Result<Value>
    where
        F: FnMut(&crate::batch::Slice) -> Result<Box<dyn WindowSource + 'a>>,
    {
        if let Err(error) = check_cancel(cancel) {
            self.output = None;
            self.failed = true;
            return Err(error);
        }
        ensure!(
            !self.failed,
            "exactextract job failed; close it and create a new job"
        );
        ensure!(
            (1..=4096).contains(&max_rows),
            "max_rows must be in 1..4096"
        );
        let attempt = (|| {
            if self.output.is_none() {
                let source_setup = std::time::Instant::now();
                let mut readers = Vec::with_capacity(self.spec.slices.len());
                let mut retained = 0_usize;
                for slice in &self.spec.slices {
                    check_cancel(cancel)?;
                    let reader = open(slice)?;
                    let bound = reader.retained_memory_bound();
                    ensure!(
                        bound <= crate::source::NATIVE_SOURCE_RETAINED_BYTES,
                        "exactextract reader exceeds its pre-admitted retained-memory bound"
                    );
                    retained = retained.checked_add(bound).ok_or_else(|| {
                        anyhow::anyhow!("exactextract retained reader bound overflow")
                    })?;
                    reader.verify_immutable()?;
                    readers.push(reader);
                }
                let source_setup_ms = source_setup.elapsed().as_secs_f64() * 1000.;
                let limits = effective_limits(&self.spec, &self.decision)?;
                let inputs: Vec<_> = readers
                    .iter()
                    .zip(&self.spec.slices)
                    .map(|(r, s)| Input {
                        source: r.as_ref(),
                        bands: s.bands.clone().unwrap_or_else(|| {
                            self.spec.options.selected_bands(r.metadata().bands.len())
                        }),
                    })
                    .collect();
                let contributions = contribution_bound(
                    &inputs,
                    self.spec.zones.len(),
                    self.spec.budget.max_contributions,
                )?;
                let zones: Vec<_> = self.spec.zones.iter().map(|z| z.geometry.clone()).collect();
                let mut output = crate::exactextract::execute(
                    &inputs,
                    &zones,
                    &self.spec.crs,
                    &self.spec.options,
                    &limits,
                    cancel,
                    cache,
                    self.spec
                        .budget
                        .working_bytes
                        .saturating_sub(retained)
                        .saturating_sub(self.spec_bytes)
                        .saturating_sub(self.geometry_reserve),
                )?;
                output.metrics["batch_source_setup_ms"] = json!(source_setup_ms);
                output.metrics["new_source_opens"] =
                    json!(self.spec.slices.iter().filter(|s| s.spec.is_some()).count());
                output.metrics["registered_source_references"] = json!(
                    self.spec
                        .slices
                        .iter()
                        .filter(|s| s.source.is_some())
                        .count()
                );
                output.metrics["reader_retained_bound_bytes"] = json!(retained);
                output.metrics["contribution_admission_upper_bound"] = json!(contributions);
                output.metrics["contribution_admission_policy"] = json!(
                    "full source grid cells times selected bands times zones; conservative admission, not measured visits"
                );
                output.metrics["batch_geometry_reserved_bytes"] = json!(self.geometry_reserve);
                ensure!(
                    output.bytes() <= self.spec.budget.output_bytes / 2,
                    "exactextract staged result exceeds half of output budget reserved for typed results"
                );
                self.output = Some(output);
            }
            let output = self.output.as_ref().expect("complete typed result");
            let stats = crate::exactextract::statistics(&self.spec.options)?;
            let total = self.spec.zones.len() * self.spec.slices.len();
            let end = self.cursor.saturating_add(max_rows).min(total);
            let mut rows = Vec::new();
            let mut charge = output.bytes().saturating_add(page_envelope_bound(
                &self.spec,
                output,
                &self.decision.provenance,
            )?);
            ensure!(
                charge <= self.spec.budget.output_bytes,
                "exactextract page envelope exceeds output budget before allocation"
            );
            for position in self.cursor..end {
                check_cancel(cancel)?;
                let si = position / self.spec.zones.len();
                let zi = position % self.spec.zones.len();
                let slice = &self.spec.slices[si];
                let zone = &self.spec.zones[zi];
                let d = &output.descriptors[si];
                let next_charge = charge
                    .saturating_add(row_bound(&self.spec, output, zi, si))
                    .saturating_add(geometry_bound(&self.decision.provenance, 0)?);
                if next_charge > self.spec.budget.output_bytes {
                    ensure!(
                        !rows.is_empty(),
                        "one exactextract row exceeds its pre-admitted output page budget"
                    );
                    break;
                }
                let id = blake3::hash(
                    format!("{}:{zi}:{si}:{}", self.fingerprint, d.source_id).as_bytes(),
                )
                .to_hex()
                .to_string();
                let mut row = json!({"result_id":id,"zone_id":zone.id,"zone_version":zone.version,"slice_id":slice.id,"bands":output.bands(zi,si,&stats)});
                if self.spec.output_mode == OutputMode::Full {
                    row["source_id"] = json!(d.source_id);
                    row["grid_id"] = json!(d.grid.identity());
                    row["time"] = json!(slice.time);
                    row["variable"] = json!(slice.variable);
                    row["statistics"] = json!(stats);
                    row["provenance"] = self.decision.provenance.clone();
                }
                charge = next_charge;
                rows.push(row);
            }
            let cursor = self.cursor + rows.len();
            let complete = cursor == total;
            let descriptor = json!({"schema":"skarve_exactextract_five_v1","statistics":stats,"provenance":self.decision.provenance,"slices":self.spec.slices.iter().zip(&output.descriptors).map(|(s,d)|json!({"slice_id":s.id,"time":s.time,"variable":s.variable,"source_id":d.source_id,"grid_id":d.grid.identity(),"bands":d.bands})).collect::<Vec<_>>()});
            let page = json!({"rows":rows,"complete":complete,"next_row":cursor,"total_rows":total,"fingerprint":self.fingerprint,"descriptor":descriptor,"provenance":self.decision.provenance,"metrics":output.metrics,"checkpoint_supported":false,"checkpoint":null});
            ensure!(
                serde_json::to_vec(&page)?.len() * 4 + output.bytes()
                    <= self.spec.budget.output_bytes,
                "exactextract page plus typed staging exceeds output budget"
            );
            self.cursor = cursor;
            Ok(page)
        })();
        if attempt.is_err() {
            self.output = None;
            self.failed = true;
        }
        attempt
    }
}
