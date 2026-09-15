//! Draft regression coverage for the optional tile-local query-mask buffer.
//! Independent integer-valued mask oracle; no source file or benchmark fixture.
use anyhow::Result;
use raster_engine::{
    batch::{BorrowedSource, Job, JobSpec, ResidentSource},
    model::{Band, Grid, Raster},
    source::{RasterMetadata, ReadMetrics, WindowSource},
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

fn fixture(version: usize) -> Raster {
    let grid = Grid {
        width: 32,
        height: 2,
        transform: [0., 1., 0., 2., 0., -1.],
        crs: "LOCAL".into(),
    };
    let bands = (0..4)
        .map(|b| {
            let values = (0..64)
                .map(|i| match b {
                    1 => [3., 1., 1., 0., -3., 3.][(i + version) % 6],
                    2 => [1., 3., -1., 0., -1., 1.][(i + version) % 6],
                    3 if i == 0 => -0.,
                    _ => (i as i32 - 32 + 3 * b as i32 + version as i32) as f64,
                })
                .collect();
            let valid = (0..64)
                .map(|i| {
                    if b == 1 {
                        (i + version) % 6 != 5
                    } else {
                        (i + b + version) % 11 != 5
                    }
                })
                .collect();
            Band {
                values,
                valid,
                unit: Some("people".into()),
            }
        })
        .collect();
    Raster {
        grid,
        bands,
        source_id: format!("independent-mask-{version}"),
    }
}

struct Tracked<'a> {
    inner: ResidentSource<'a>,
    reads: Cell<usize>,
    cancel_after_read: Option<&'a AtomicBool>,
}
impl<'a> Tracked<'a> {
    fn new(raster: &'a Raster, cancel_after_read: Option<&'a AtomicBool>) -> Self {
        Self {
            inner: ResidentSource::new(raster),
            reads: Cell::new(0),
            cancel_after_read,
        }
    }
}
impl WindowSource for Tracked<'_> {
    fn metadata(&self) -> &RasterMetadata {
        self.inner.metadata()
    }
    fn verify_immutable(&self) -> Result<()> {
        self.inner.verify_immutable()
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
        let value = self
            .inner
            .read_selected_window_cancellable(x, y, w, h, bands, max, cancel)?;
        self.reads.set(self.reads.get() + 1);
        if let Some(flag) = self.cancel_after_read {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(value)
    }
}

fn spec(bands: &[usize], tile_bytes: usize, two_sources: bool) -> JobSpec {
    let mut slices = vec![json!({"id":"a","source":"a","bands":bands})];
    if two_sources {
        slices.push(json!({"id":"b","source":"b","bands":bands}));
    }
    serde_json::from_value(json!({
        "zones":[{"id":"whole","version":"v1","geometry":{"type":"Polygon","coordinates":[[[0.,0.],[32.,0.],[32.,2.],[0.,2.],[0.,0.]]]}}],
        "slices":slices,"crs":"LOCAL","schedule":"tile",
        "geometry_layout":"compact","tile_edge":32,"window_policy":"fixed","output_mode":"full",
        "options":{"statistics":["sum","support","mean","min","max"]},
        "mask":{"op":"and","left":{"op":"valid","input":{"op":"band","band":1}},
            "right":{"op":"greater","left":{"op":"normalized_difference",
                "left":{"op":"band","band":1},"right":{"op":"band","band":2}},
                "right":{"op":"constant","value":0.}}},
        "budget":{"tile_bytes":tile_bytes}
    })).unwrap()
}

// No expression evaluator is used by this oracle. In the authored six-state
// mask, normalized difference is positive only for (3,1) and (-3,-1).
// Zero scale, zero denominator, false comparison and invalid dependencies reject.
fn check_oracle(row: &Value, raster: &Raster, version: usize, selected: &[usize]) {
    assert_eq!(row["zone_id"], "whole");
    assert_eq!(row["bands"].as_array().unwrap().len(), selected.len());
    for (result, &b) in row["bands"].as_array().unwrap().iter().zip(selected) {
        assert_eq!(result["band"], b);
        let mut values = vec![];
        for i in 0..64 {
            let mask = matches!((i + version) % 6, 0 | 4)
                && raster.bands[1].valid[i]
                && raster.bands[2].valid[i];
            if mask && raster.bands[b].valid[i] {
                values.push(raster.bands[b].values[i]);
            }
        }
        let sum: f64 = values.iter().sum();
        assert_eq!(
            result["fractional_sum"].as_f64().unwrap().to_bits(),
            sum.to_bits()
        );
        assert_eq!(result["covered_cell_equivalents"], values.len() as f64);
        assert_eq!(result["valid_cell_count"], values.len());
        assert_eq!(result["intersecting_cell_count"], 64);
        assert_eq!(result["selected_cell_equivalents"], 64.);
        assert_eq!(
            result["missing_cell_equivalents"],
            (64 - values.len()) as f64
        );
        assert_eq!(result["outside_cell_equivalents"], 0.);
        assert_eq!(
            result["coverage_weighted_mean"].as_f64().unwrap().to_bits(),
            (sum / values.len() as f64).to_bits()
        );
        assert_eq!(
            result["min"],
            values.iter().copied().fold(f64::INFINITY, f64::min)
        );
        assert_eq!(
            result["max"],
            values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        );
        assert_eq!(result["status"], "partial");
        // Existing mask-only result metadata retains the original source unit.
        assert_eq!(result["unit"], "people");
    }
}

fn collect(job: &mut Job, a: &Tracked<'_>, b: &Tracked<'_>, cancel: &AtomicBool) -> Vec<Value> {
    let mut rows = vec![];
    loop {
        let page = job
            .next(
                128,
                |slice| {
                    Ok(Box::new(BorrowedSource(if slice.id == "a" {
                        a
                    } else {
                        b
                    })))
                },
                cancel,
            )
            .unwrap();
        rows.extend(page["rows"].as_array().unwrap().clone());
        if page["complete"] == true {
            break;
        }
    }
    rows
}

#[test]
fn mask_reuse_matches_independent_band_oracle_and_refreshes_each_source() {
    let ra = fixture(0);
    let rb = fixture(1);
    let a = Tracked::new(&ra, None);
    let b = Tracked::new(&rb, None);
    let selected = [3, 0];
    let mut job = Job::new(spec(&selected, 1 << 20, true), None, 32_768, 1 << 30).unwrap();
    let rows = collect(&mut job, &a, &b, &AtomicBool::new(false));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["slice_id"], "a");
    assert_eq!(rows[1]["slice_id"], "b");
    check_oracle(&rows[0], &ra, 0, &selected);
    check_oracle(&rows[1], &rb, 1, &selected);
    assert!(a.reads.get() > 0 && b.reads.get() > 0);
    assert_eq!(job.metrics.mask_expression_passes, 2);
    assert_eq!(job.metrics.mask_expression_cells, 128);
    assert_eq!(
        job.metrics.shared_mask_peak_bytes,
        64 + std::mem::size_of::<Vec<u8>>()
    );
}

#[test]
fn tight_admission_keeps_old_mask_path_and_below_base_rejects_before_read() {
    let raster = fixture(0);
    let source = Tracked::new(&raster, None);
    let selected = [3, 0];
    let input = source.read_buffer_bound(32, 2, &[0, 1, 2, 3]).unwrap();
    let base = input + 64 * 9 * selected.len();
    let mut old_capacity = Job::new(spec(&selected, base, false), None, 32_768, 1 << 30).unwrap();
    let fallback = collect(&mut old_capacity, &source, &source, &AtomicBool::new(false));
    check_oracle(&fallback[0], &raster, 0, &selected);
    let mask_capacity = base + 64 + std::mem::size_of::<Vec<u8>>();
    let mut with_mask =
        Job::new(spec(&selected, mask_capacity, false), None, 32_768, 1 << 30).unwrap();
    let shared = collect(&mut with_mask, &source, &source, &AtomicBool::new(false));
    assert_eq!(fallback[0]["bands"], shared[0]["bands"]);
    assert_eq!(old_capacity.metrics.shared_mask_peak_bytes, 0);
    assert_eq!(old_capacity.metrics.mask_expression_cells, 128);
    assert_eq!(with_mask.metrics.mask_expression_cells, 64);
    assert_eq!(
        with_mask.metrics.shared_mask_peak_bytes,
        mask_capacity - base
    );
    let before = source.reads.get();
    let mut refused = Job::new(spec(&selected, base - 1, false), None, 32_768, 1 << 30).unwrap();
    let error = refused
        .next(
            128,
            |_| Ok(Box::new(BorrowedSource(&source))),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("batch tile allocation budget"));
    assert_eq!(source.reads.get(), before);
    assert_eq!(refused.metrics.output_rows, 0);
}

#[test]
fn single_output_still_uses_original_evaluation_contract() {
    let raster = fixture(0);
    let source = Tracked::new(&raster, None);
    let mut job = Job::new(spec(&[3], 1 << 20, false), None, 32_768, 1 << 30).unwrap();
    let rows = collect(&mut job, &source, &source, &AtomicBool::new(false));
    check_oracle(&rows[0], &raster, 0, &[3]);
}

#[test]
fn cancellation_after_read_publishes_no_masked_rows() {
    let raster = fixture(0);
    let cancel = AtomicBool::new(false);
    let source = Tracked::new(&raster, Some(&cancel));
    let mut job = Job::new(spec(&[3, 0], 1 << 20, false), None, 32_768, 1 << 30).unwrap();
    let error = job
        .next(128, |_| Ok(Box::new(BorrowedSource(&source))), &cancel)
        .unwrap_err();
    assert!(error.to_string().contains("cancel"));
    assert_eq!(source.reads.get(), 1);
    assert_eq!(job.metrics.raw_band_cell_visits, 0);
    assert_eq!(job.metrics.output_rows, 0);
    assert_eq!(job.checkpoint().next_row, 0);
}

#[test]
fn selected_negative_zero_survives_shared_mask_without_other_output_samples() {
    let mut raster = fixture(0);
    raster.bands[3].values.fill(0.);
    raster.bands[3].valid.fill(false);
    raster.bands[3].values[0] = -0.;
    raster.bands[3].valid[0] = true;
    raster.bands[1].values[0] = 3.;
    raster.bands[1].valid[0] = true;
    raster.bands[2].values[0] = 1.;
    raster.bands[2].valid[0] = true;
    let source = Tracked::new(&raster, None);
    // Two outputs and generous tile admission reach the draft shared-mask path.
    let mut shared_job = Job::new(spec(&[3, 0], 1 << 20, false), None, 32_768, 1 << 30).unwrap();
    let shared = collect(&mut shared_job, &source, &source, &AtomicBool::new(false));
    let actual = &shared[0]["bands"][0];
    assert_eq!(actual["band"], 3);
    assert_eq!(actual["valid_cell_count"], 1);
    assert_eq!(actual["covered_cell_equivalents"], 1.);
    for name in ["min", "max"] {
        assert_eq!(
            actual[name].as_f64().unwrap().to_bits(),
            (-0.0f64).to_bits(),
            "{name} erased selected negative zero"
        );
        assert_ne!(actual[name].as_f64().unwrap().to_bits(), 0.0f64.to_bits());
    }
    // The sole selected sample is independently known, and the unchanged
    // single-output evaluation path is an additional same-policy control.
    let mut single_job = Job::new(spec(&[3], 1 << 20, false), None, 32_768, 1 << 30).unwrap();
    let single = collect(&mut single_job, &source, &source, &AtomicBool::new(false));
    for name in [
        "fractional_sum",
        "covered_cell_equivalents",
        "coverage_weighted_mean",
        "min",
        "max",
    ] {
        assert_eq!(
            actual[name].as_f64().unwrap().to_bits(),
            single[0]["bands"][0][name].as_f64().unwrap().to_bits()
        );
    }
    assert_eq!(actual["unit"], "people");
}
