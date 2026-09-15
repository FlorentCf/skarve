//! Owned, synchronous Rust consumer interface over the ordinary engine protocol.
//!
//! Raster samples stay inside the readers and reducers. GeoJSON, query controls
//! and result metadata use `serde_json::Value`; no new numerical path is added.
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A cloneable cooperative cancellation signal. It does not terminate threads.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    /// Interrupt current work at the engine's next cancellation checkpoint.
    /// Cancellation stays set until [`Skarve::reset_cancellation`] is called.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Common native query controls. Band indexes are zero-based exposed bands.
#[derive(Clone, Default)]
pub struct CarveOptions {
    /// Empty selects every exposed band, in source order.
    pub bands: Vec<usize>,
    /// Empty uses the engine's default statistics. Unknown names are rejected.
    pub metrics: Vec<String>,
    /// If omitted, coordinates are asserted to use the source CRS. No reprojection.
    pub crs: Option<String>,
}

/// An owned engine session with one synchronous operation at a time.
///
/// A source or batch mutably borrows the session; Rust prevents its destruction
/// or another session operation while that handle is live. Dropping a handle
/// closes its native registration. For concurrency, use separate sessions and
/// bound their combined resources in the application.
#[derive(Default)]
pub struct Skarve {
    session: crate::session::Session,
    cancellation: Cancellation,
    sequence: u64,
}
impl Skarve {
    /// Create an empty session. No source is opened until [`Self::infuse`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Obtain a signal that another thread may set while this thread is working.
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    /// Clear cancellation between operations, after interrupted work has returned.
    pub fn reset_cancellation(&mut self) {
        self.cancellation.0.store(false, Ordering::Relaxed);
    }

    /// Execute an advanced ordinary protocol request.
    ///
    /// This retains the normal request-size and control-allocation reservation,
    /// unlike calling the expert low-level `Session::call` directly. Geometry and
    /// small controls cross JSON; source raster arrays do not. Unsupported fields
    /// and incompatible numerical policies are rejected by the existing engine.
    pub fn request(&mut self, request: Value) -> Result<Value> {
        self.call(request, false)
    }

    fn call(&mut self, request: Value, cleanup: bool) -> Result<Value> {
        let encoded = serde_json::to_string(&request)?;
        let uncancelled = AtomicBool::new(false);
        let cancel = if cleanup {
            &uncancelled
        } else {
            &self.cancellation.0
        };
        let response = crate::session::request(&mut self.session, &encoded, cancel);
        let mut envelope: Value =
            serde_json::from_str(&response).context("invalid native result envelope")?;
        ensure!(
            envelope["ok"] == true,
            "{}",
            envelope["error"]
                .as_str()
                .unwrap_or("native operation failed")
        );
        Ok(envelope["result"].take())
    }

    fn id(&mut self) -> Result<String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("session handle sequence exhausted")?;
        Ok(format!("__skarve_rust_{}", self.sequence))
    }

    /// Open a local TIFF/COG/SKV path or an allowed HTTPS location.
    ///
    /// The source reader detects SKV; explicit formats, identity pins, band maps,
    /// overviews and HTTP limits are available through [`Self::infuse_spec`].
    /// Applications must enforce their own source allowlist for untrusted users.
    pub fn infuse(&mut self, location: impl AsRef<str>) -> Result<Source<'_>> {
        self.infuse_spec(json!({"location":location.as_ref()}))
    }

    /// Open an explicit source specification using the ordinary installed schema.
    /// Locations and credentials are intentionally not exposed by `Debug`.
    pub fn infuse_spec(&mut self, spec: Value) -> Result<Source<'_>> {
        let id = self.id()?;
        let metadata = self.request(json!({"op":"register_source", "id":id, "spec":spec}))?;
        Ok(Source {
            engine: self,
            id,
            metadata,
            closed: false,
        })
    }

    /// Start a genuine shared-scan polygon × raster-slice job.
    ///
    /// `job` uses the documented `zones`, `slices`, `crs`, `options` and `budget`
    /// schema. Native pages are requested only when the iterator advances. No
    /// single-polygon operation is used to implement batching. A page remains
    /// owned by the caller; retaining all pages requires caller memory budgeting.
    pub fn cleave(&mut self, job: Value, max_rows: usize) -> Result<Batch<'_>> {
        ensure!(
            (1..=4096).contains(&max_rows),
            "max_rows must be in 1..4096"
        );
        let id = self.id()?;
        self.request(json!({"op":"start_job", "id":id, "job":job}))?;
        Ok(Batch {
            engine: self,
            id,
            max_rows,
            complete: false,
            closed: false,
        })
    }
}

/// A registered source borrowing its engine until closed or dropped.
pub struct Source<'a> {
    engine: &'a mut Skarve,
    id: String,
    metadata: Value,
    closed: bool,
}
impl Source<'_> {
    /// Metadata observed on source registration; use [`Self::inspect`] to verify
    /// the source generation again and obtain current diagnostics.
    pub fn metadata(&self) -> &Value {
        &self.metadata
    }

    /// Recheck source identity and obtain metadata/access diagnostics.
    pub fn inspect(&mut self) -> Result<Value> {
        self.engine
            .request(json!({"op":"source_info", "source":self.id}))
    }

    /// Compute native fractional statistics without preparing or converting data.
    pub fn carve(&mut self, zone: &Value, options: &CarveOptions) -> Result<Value> {
        let mut request = json!({"bands":options.bands});
        if !options.metrics.is_empty() {
            request["statistics"] = json!(options.metrics);
        }
        if let Some(crs) = &options.crs {
            request["crs"] = json!(crs);
        }
        self.carve_with_options(zone, request)
    }

    /// Query with advanced options, including explicitly selected backend/policy.
    /// The engine validates eligibility; this function never silently translates
    /// policies or removes unknown fields.
    pub fn carve_with_options(&mut self, zone: &Value, mut options: Value) -> Result<Value> {
        let object = options
            .as_object_mut()
            .context("query options must be an object")?;
        ensure!(
            !object.contains_key("op")
                && !object.contains_key("source")
                && !object.contains_key("geometry"),
            "query options cannot override operation, source or geometry"
        );
        object.insert("op".into(), json!("measure_source"));
        object.insert("source".into(), json!(self.id));
        object.insert("geometry".into(), zone.clone());
        if !object.contains_key("crs") {
            let crs = self.metadata["metadata"]["grid"]["crs"]
                .as_str()
                .ok_or_else(|| anyhow!("source CRS is missing"))?;
            object.insert("crs".into(), json!(crs));
        }
        self.engine.request(options)
    }

    /// Compile this exact supported source view to self-contained lossless SKV.
    /// Existing output paths are rejected. The original is not needed to serve
    /// the completed SKV; format/writer behavior is unchanged.
    pub fn compile(
        &mut self,
        output: impl AsRef<str>,
        options: &crate::skv::CompileOptions,
    ) -> Result<Value> {
        self.engine
            .request(json!({"op":"compile_source", "source":self.id,
                                   "output":output.as_ref(), "options":options}))
    }

    /// Close deterministically and report any native cleanup error.
    pub fn close(mut self) -> Result<()> {
        self.close_inner()
    }
    fn close_inner(&mut self) -> Result<()> {
        if !self.closed {
            self.engine
                .call(json!({"op":"close_source", "source":self.id}), true)?;
            self.closed = true;
        }
        Ok(())
    }
}
impl Drop for Source<'_> {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

/// Backpressured pages from the engine's bounded shared batch executor.
/// Dropping early releases the job. After the first error no further work runs.
pub struct Batch<'a> {
    engine: &'a mut Skarve,
    id: String,
    max_rows: usize,
    complete: bool,
    closed: bool,
}
impl Batch<'_> {
    /// Cancel consumption and release the job, reporting cleanup failure.
    pub fn close(mut self) -> Result<()> {
        self.close_inner()
    }
    fn close_inner(&mut self) -> Result<()> {
        if !self.closed {
            self.engine
                .call(json!({"op":"close_job", "id":self.id}), true)?;
            self.closed = true;
        }
        Ok(())
    }
}
impl Iterator for Batch<'_> {
    type Item = Result<Value>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.complete || self.closed {
            return None;
        }
        let result = self.engine.request(json!({"op":"next_job", "id":self.id,
            "max_rows":self.max_rows, "include_checkpoint":true}));
        self.complete = result
            .as_ref()
            .map_or(true, |page| page["complete"] == true);
        if self.complete {
            let cleanup = self.close_inner();
            if result.is_ok() {
                if let Err(error) = cleanup {
                    return Some(Err(error));
                }
            }
        }
        Some(result)
    }
}
impl std::iter::FusedIterator for Batch<'_> {}
impl Drop for Batch<'_> {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}
