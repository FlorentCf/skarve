use crate::{aggregate::*, coverage::*, model::*};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::atomic::AtomicBool, time::Instant};

#[derive(Default)]
pub struct Session {
    sources: HashMap<String, Raster>,
    plans: HashMap<String, Plan>,
    indexes: HashMap<String, Prepared>,
    hierarchies: HashMap<String, crate::hierarchy::Hierarchy>,
    cumulative: HashMap<(String, String), crate::cumulative::CumulativeField>,
    transient_reserve: usize,
    files: HashMap<String, RegisteredFile>,
    tile_cache: crate::tile_cache::TileCache,
    readers: HashMap<String, Box<dyn crate::source::WindowSource>>,
    jobs: HashMap<String, crate::batch::Job>,
    exactextract_jobs: HashMap<String, crate::exactextract_job::Job>,
    job_provenance: HashMap<String, Value>,
    retained_indexes: HashMap<String, RegisteredIndex>,
}
struct RegisteredIndex {
    source: String,
    index: crate::persistent::PersistedIndex,
    read_memory_bytes: usize,
}
struct RegisteredFile {
    path: String,
    signature: String,
    source: Option<crate::io::LocalSource>,
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string field {key}"))
}
fn options(v: &Value) -> Result<Options> {
    Ok(Options {
        bands: v
            .get("bands")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?
            .unwrap_or_default(),
        histogram_edges: v
            .get("histogram_edges")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
        weight_band: v
            .get("weight_band")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
        category_values: v
            .get("category_values")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
        quantiles: v
            .get("quantiles")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
        quantile_max_samples: v
            .get("quantile_max_samples")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
        statistics: v
            .get("statistics")
            .map(|x| serde_json::from_value(x.clone()))
            .transpose()?,
    })
}
impl Session {
    pub(crate) fn reader(&self, id: &str) -> Result<&dyn crate::source::WindowSource> {
        self.readers
            .get(id)
            .map(|s| s.as_ref())
            .ok_or_else(|| anyhow::anyhow!("unknown reader source"))
    }
    pub fn bytes(&self) -> usize {
        self.sources.values().map(Raster::bytes).sum::<usize>()
            + self.indexes.values().map(|p| p.bytes).sum::<usize>()
            + self.hierarchies.values().map(|p| p.bytes).sum::<usize>()
            + self.cumulative.values().map(|p| p.bytes).sum::<usize>()
            + self.plans.values().map(Plan::bytes).sum::<usize>()
            + self.tile_cache.limit().max(self.tile_cache.bytes())
            + self
                .jobs
                .values()
                .map(crate::batch::Job::reservation)
                .sum::<usize>()
            + self
                .readers
                .values()
                .map(|source| source.retained_memory_bound().max(16 * 1024 * 1024))
                .sum::<usize>()
            + self
                .exactextract_jobs
                .values()
                .map(crate::exactextract_job::Job::reservation)
                .sum::<usize>()
            + self.job_provenance.len() * 65536
            + self.retained_indexes.len()
                * (crate::persistent::PersistedIndex::RETAINED_BYTES + 65536)
            + (self.files.len()
                + self.sources.len()
                + self.plans.len()
                + self.indexes.len()
                + self.hierarchies.len()
                + self.cumulative.len())
                * 65_536
    }
    pub(crate) fn available(&self) -> usize {
        MAX_BYTES
            .saturating_sub(self.bytes())
            .saturating_sub(self.transient_reserve)
    }
    pub fn call(&mut self, v: Value, cancel: &AtomicBool) -> Result<Value> {
        check_cancel(cancel)?;
        let start = Instant::now();
        let op = text(&v, "op")?;
        for field in ["id", "source", "plan", "index_handle"] {
            if let Some(value) = v.get(field).and_then(Value::as_str) {
                ensure!(value.len() <= 1024, "{field} exceeds 1024 bytes");
            }
        }
        let allowed: &[&str] = match op {
            "register_source" => &["op", "id", "spec"],
            "source_info" | "source_metrics" | "begin_source_query" | "close_source" => {
                &["op", "source"]
            }
            "begin_verified_source_query" => &["op", "source", "renew_budget"],
            "end_verified_source_query" => &["op", "source"],
            "compile_source" => &["op", "source", "output", "options"],
            "measure_ordered_source" => &["op", "source", "request", "numerical_policy"],
            "measure_ordered_profile" => &[
                "op",
                "profile",
                "request",
                "numerical_policy",
                "view_id",
                "access_class",
            ],
            "verify_skv" => &["op", "spec"],
            "register_index" => &[
                "op",
                "id",
                "source",
                "index",
                "expected_build_id",
                "read_memory_bytes",
            ],
            "index_info" | "close_index" => &["op", "id"],
            "measure_hm_population" => &[
                "op",
                "source",
                "geometry",
                "index",
                "expected_build_id",
                "read_memory_bytes",
            ],
            "prepare_source" => &[
                "op",
                "source",
                "index",
                "tile_edge",
                "layout",
                "summary_backend",
                "boundary_source",
            ],
            "measure_source" => &[
                "op",
                "source",
                "geometry",
                "crs",
                "bands",
                "histogram_edges",
                "weight_band",
                "statistics",
                "category_values",
                "quantiles",
                "quantile_max_samples",
                "index",
                "index_handle",
                "expected_build_id",
                "read_memory_bytes",
                "forbidden_raw_tiles",
                "joint_planner",
                "backend",
                "numerical_policy",
                "accepted_policies",
                "execution_envelope",
                "backend_options",
            ],
            "start_job" => &["op", "id", "job", "checkpoint"],
            "next_job" => &["op", "id", "max_rows", "include_checkpoint"],
            "job_info" | "close_job" => &["op", "id"],
            "open" => &["op", "id", "raster", "path"],
            "close" => &["op", "source"],
            "prepare" => &["op", "source", "backend", "tile_edge", "columns", "bands"],
            "prepare_file" => &[
                "op",
                "path",
                "index",
                "tile_edge",
                "layout",
                "summary_backend",
                "boundary_source",
            ],
            "drop_plan" => &["op", "plan"],
            "stats" | "backends" => &["op"],
            "register_file" => &["op", "id", "path"],
            "close_file" => &["op", "source"],
            "configure_file_cache" => &["op", "bytes"],
            "clear_file_cache" => &["op"],
            "compile" => &[
                "op",
                "source",
                "id",
                "geometry",
                "crs",
                "strategy",
                "debug_cells",
            ],
            "measure" => &[
                "op",
                "source",
                "plan",
                "geometry",
                "crs",
                "strategy",
                "bands",
                "histogram_edges",
                "weight_band",
                "use_index",
                "statistics",
                "category_values",
                "quantiles",
                "quantile_max_samples",
                "backend",
                "numerical_policy",
                "accepted_policies",
                "execution_envelope",
                "backend_options",
            ],
            "measure_file" | "measure_registered_file" => &[
                "op",
                "path",
                "source",
                "geometry",
                "crs",
                "bands",
                "histogram_edges",
                "weight_band",
                "statistics",
                "category_values",
                "quantiles",
                "quantile_max_samples",
                "index",
                "raw_path",
                "expected_build_id",
                "read_memory_bytes",
                "summary_page_bytes",
                "coalesce_raw",
                "order_summaries",
                "joint_planner",
                "forbidden_raw_tiles",
                "backend",
            ],
            _ => bail!("unknown operation"),
        };
        ensure!(
            v.as_object().is_some_and(|m| m
                .keys()
                .all(|k| k == "mode" || allowed.contains(&k.as_str()))),
            "unknown request field; unsupported options are not ignored"
        );
        for flag in ["debug_cells", "use_index"] {
            if let Some(value) = v.get(flag) {
                ensure!(value.is_boolean(), "{flag} must be boolean");
            }
        }
        if let Some(strategy) = v.get("strategy") {
            ensure!(strategy.is_string(), "strategy must be a string");
            ensure!(
                ["scanline", "direct"].iter().any(|s| strategy == s),
                "unknown coverage strategy"
            );
        }
        for name in [
            "backend",
            "layout",
            "index",
            "index_handle",
            "raw_path",
            "expected_build_id",
        ] {
            if let Some(value) = v.get(name) {
                ensure!(value.is_string(), "{name} must be a string");
            }
        }
        if v.get("plan").is_some() {
            ensure!(
                v.get("geometry").is_none()
                    && v.get("crs").is_none()
                    && v.get("strategy").is_none(),
                "plan and geometry/strategy selection are mutually exclusive"
            );
        }
        if op == "open" {
            ensure!(
                v.get("path").is_some() != v.get("raster").is_some(),
                "provide exactly one of path or raster"
            );
        }
        if op == "measure_hm_population" {
            ensure!(
                v.get("mode").and_then(Value::as_str) == Some(crate::hm_compat::POLICY),
                "measure_hm_population requires its explicit spherical population policy"
            );
        } else if let Some(mode) = v.get("mode") {
            ensure!(mode == "native_grid_planar", "unsupported numerical mode");
        }
        match text(&v, "op")? {
            "backends" => Ok(
                json!({"native":true,"exactextract":cfg!(feature="exactextract"),"native_policy":crate::backend::NATIVE_POLICY,"exactextract_policy":crate::backend::EXACTEXTRACT_POLICY,"exactextract_policies":[crate::backend::EXACTEXTRACT_POLICY,crate::backend::EXACTEXTRACT_RASTERIO_POLICY],"default_backend":"native","default_auto_policy":"native_only","execution_envelopes":["embedded_cooperative"]}),
            ),
            "register_index" => {
                let id = text(&v, "id")?.to_owned();
                ensure!(
                    !self.retained_indexes.contains_key(&id) && self.retained_indexes.len() < 16,
                    "index ID already exists or retained index count budget exceeded"
                );
                ensure!(
                    self.available() >= crate::persistent::PersistedIndex::RETAINED_BYTES + 65536,
                    "insufficient session retained index memory"
                );
                let source_id = text(&v, "source")?;
                let source = self
                    .readers
                    .get(source_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                let path = text(&v, "index")?;
                let index_path = if path.ends_with(".rsi") || path.contains("://") {
                    path.to_owned()
                } else {
                    format!("{path}/summary.rsi")
                };
                let memory = v
                    .get("read_memory_bytes")
                    .map(|n| serde_json::from_value(n.clone()))
                    .transpose()?
                    .unwrap_or(32 * 1024 * 1024);
                let index = crate::persistent::PersistedIndex::open(
                    &index_path,
                    v["expected_build_id"].as_str(),
                    memory,
                    cancel,
                )?;
                ensure!(
                    index.header().boundary_source == crate::persistent::BoundarySource::Original,
                    "retained index requires original boundary source"
                );
                index.verify_source(source.as_ref())?;
                let result = json!({"index_handle":id,"source":source_id,"header":index.header(),
                    "retained_bound_bytes":crate::persistent::PersistedIndex::RETAINED_BYTES,
                    "query_read_memory_bytes":memory,"retained_value_bytes":0,"retained_summary_page_bytes":0});
                self.retained_indexes.insert(
                    id,
                    RegisteredIndex {
                        source: source_id.to_owned(),
                        index,
                        read_memory_bytes: memory,
                    },
                );
                Ok(result)
            }
            "index_info" => {
                let index = self
                    .retained_indexes
                    .get_mut(text(&v, "id")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown retained index"))?;
                index.index.inspect_retained(cancel)
            }
            "close_index" => {
                ensure!(
                    self.retained_indexes.remove(text(&v, "id")?).is_some(),
                    "unknown retained index"
                );
                Ok(json!({"closed":true}))
            }
            "measure_hm_population" => {
                ensure!(
                    self.available() >= crate::hm_compat::RESERVATION_BYTES,
                    "insufficient session spherical population query memory"
                );
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                ensure!(
                    v.get("expected_build_id").is_none() || v.get("index").is_some(),
                    "expected_build_id requires index"
                );
                crate::hm_compat::measure(
                    source.as_ref(),
                    &v["geometry"],
                    v["index"].as_str(),
                    v["expected_build_id"].as_str(),
                    v.get("read_memory_bytes")
                        .map(|n| serde_json::from_value(n.clone()))
                        .transpose()?
                        .unwrap_or(32 * 1024 * 1024),
                    cancel,
                )
            }
            "register_source" => {
                let id = text(&v, "id")?.to_owned();
                ensure!(
                    !self.readers.contains_key(&id)
                        && !self.sources.contains_key(&id)
                        && self.readers.len() < 32,
                    "source ID already exists or reader count budget exceeded"
                );
                let spec: crate::source::SourceSpec = serde_json::from_value(v["spec"].clone())?;
                ensure!(
                    self.available()
                        >= (32usize * 1024 * 1024)
                            .saturating_add(spec.http.cache_bytes.saturating_sub(8 * 1024 * 1024)),
                    "insufficient session reader memory"
                );
                let source = crate::io::open_source(&spec, cancel)?;
                ensure!(
                    source.retained_memory_bound().max(16 * 1024 * 1024) <= self.available(),
                    "source retained capacity exceeds remaining session budget"
                );
                let result = json!({"source":id,"metadata":source.metadata(),"rawMetadata":crate::source_buffer::metadata(source.as_ref()),"identity":source.identity_descriptor(),"registered_http_identity":source.registered_http_identity(),"access_layout":source.access_layout(),"diagnostics":source.diagnostics()});
                self.readers.insert(id, source);
                Ok(result)
            }
            "source_info" => {
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                source.verify_immutable()?;
                Ok(
                    json!({"metadata":source.metadata(),"rawMetadata":crate::source_buffer::metadata(source.as_ref()),"identity":source.identity_descriptor(),"access_layout":source.access_layout(),"diagnostics":source.diagnostics(),
                        "serving_profile_view":source.raw_metadata().map(|raw|crate::serving_profile::expected_view(&source.metadata().grid,raw))}),
                )
            }
            "source_metrics" | "begin_source_query" => {
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                if op == "begin_source_query" {
                    source.begin_query_budget()?;
                }
                // Metrics are observational, not proof of current identity.
                // Ordinary value operations retain pre/post verification.
                Ok(json!({"diagnostics": source.diagnostics()}))
            }
            "begin_verified_source_query" | "end_verified_source_query" => {
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                if op == "begin_verified_source_query" {
                    let renew: bool = v
                        .get("renew_budget")
                        .map(|v| serde_json::from_value(v.clone()))
                        .transpose()?
                        .unwrap_or(true);
                    source.begin_verified_query(renew)?;
                } else {
                    source.end_verified_query()?;
                }
                // This proof is emitted only by this successful finalization,
                // never by metrics/registration. The exact handle prevents an
                // application's distinct object dependencies sharing it merely
                // because two objects happen to have equal ETags and lengths.
                let verification = if op == "end_verified_source_query" {
                    source.registered_http_identity().map(|identity| {
                        json!({"schema":"skarve_http_query_verification_v1",
                            "source":text(&v,"source").expect("validated source"),
                            "identity":identity})
                    })
                } else {
                    None
                };
                Ok(json!({"diagnostics": source.diagnostics(),
                    "verified_query_complete": op == "end_verified_source_query",
                    "verification": verification}))
            }
            "compile_source" => {
                let options: crate::skv::CompileOptions = v
                    .get("options")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?
                    .unwrap_or_default();
                ensure!(
                    options.working_bytes <= self.available(),
                    "insufficient session memory for SKV compiler"
                );
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                // The compiler brackets conversion with source verification and
                // owns its no-overwrite temporary output until atomic completion.
                crate::skv::compile(source.as_ref(), text(&v, "output")?, &options, cancel)
            }
            "measure_ordered_source" => {
                ensure!(
                    v["numerical_policy"].as_str() == Some("hm_demographics_ordered_v1"),
                    "ordered source execution requires explicit hm_demographics_ordered_v1 policy"
                );
                ensure!(
                    v.get("mode").is_none(),
                    "ordered source uses its explicit numerical_policy only"
                );
                // The ordinary request boundary charges the parsed control at
                // 64x its encoded length. The executor additionally charges its
                // owned plan, partials, output and bounded physical reads.
                let request: crate::ordered_source::OrderedRequest =
                    serde_json::from_value(v["request"].clone())?;
                ensure!(
                    crate::ordered_source::reservation_bytes(&request)? <= self.available(),
                    "insufficient session memory for ordered source execution"
                );
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                crate::ordered_source::execute(source.as_ref(), &request, cancel)
            }
            "measure_ordered_profile" => {
                let policy = text(&v, "numerical_policy")?;
                let view_id = text(&v, "view_id")?;
                let access = v
                    .get("access_class")
                    .map(|value| {
                        value
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("access_class must be a string"))
                    })
                    .transpose()?;
                let request: crate::ordered_source::OrderedRequest =
                    serde_json::from_value(v["request"].clone())?;
                crate::serving_profile::execute(
                    &v["profile"],
                    &request,
                    policy,
                    view_id,
                    access,
                    self.available(),
                    cancel,
                )
            }
            "verify_skv" => {
                ensure!(
                    self.available() >= crate::skv::VERIFY_WORKING_BYTES,
                    "insufficient session memory for full SKV verification"
                );
                let spec: crate::source::SourceSpec = serde_json::from_value(v["spec"].clone())?;
                crate::skv::verify(&spec, cancel)
            }
            "close_source" => {
                ensure!(
                    self.readers.remove(text(&v, "source")?).is_some(),
                    "unknown reader source"
                );
                Ok(json!({"closed":true}))
            }
            "prepare_source" => {
                ensure!(
                    self.available() >= 256 * 1024 * 1024,
                    "insufficient session builder memory"
                );
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                crate::persistent::build_from_source(
                    source.as_ref(),
                    text(&v, "index")?,
                    v.get("tile_edge")
                        .map(|n| serde_json::from_value(n.clone()))
                        .transpose()?
                        .unwrap_or(64),
                    v["layout"].as_str().unwrap_or("band_major"),
                    v["summary_backend"].as_str().unwrap_or("hierarchy"),
                    v.get("boundary_source")
                        .map(|n| serde_json::from_value(n.clone()))
                        .transpose()?
                        .unwrap_or(crate::persistent::BoundarySource::Original),
                    cancel,
                )
            }
            "measure_source" => {
                let available = self.available();
                let decision = crate::backend::resolve(
                    &v,
                    v.get("index").is_some() || v.get("index_handle").is_some(),
                )?;
                let source = self
                    .readers
                    .get(text(&v, "source")?)
                    .ok_or_else(|| anyhow::anyhow!("unknown reader source"))?;
                let o = options(&v)?;
                if decision.selected == crate::backend::Backend::Exactextract {
                    ensure!(
                        [
                            "joint_planner",
                            "expected_build_id",
                            "read_memory_bytes",
                            "forbidden_raw_tiles",
                            "mode",
                            "strategy",
                            "use_index"
                        ]
                        .iter()
                        .all(|k| v.get(k).is_none()),
                        "native index/mode/strategy options cannot be used with exactextract"
                    );
                    let input = crate::exactextract::Input {
                        source: source.as_ref(),
                        bands: o.selected_bands(source.metadata().bands.len()),
                    };
                    let output = crate::exactextract::execute(
                        &[input],
                        std::slice::from_ref(&v["geometry"]),
                        text(&v, "crs")?,
                        &o,
                        &decision.options,
                        cancel,
                        &mut self.tile_cache,
                        available,
                    )?;
                    let stats = crate::exactextract::statistics(&o)?;
                    return Ok(
                        json!({"bands":output.bands(0,0,&stats),"statistics":stats,"grid":source.metadata().grid,"source_id":source.metadata().source_id,"mode":"exactextract_fractional","provenance":decision.provenance,"work":output.metrics,"source_access":source.diagnostics(),"timing_ms":{"total":start.elapsed().as_secs_f64()*1000.}}),
                    );
                }
                if v.get("index").is_none() && v.get("index_handle").is_none() {
                    ensure!(
                        [
                            "joint_planner",
                            "expected_build_id",
                            "read_memory_bytes",
                            "forbidden_raw_tiles"
                        ]
                        .iter()
                        .all(|k| v.get(k).is_none()),
                        "persistent options require index"
                    );
                }
                let mut result = if let Some(handle) = v["index_handle"].as_str() {
                    ensure!(
                        [
                            "index",
                            "expected_build_id",
                            "read_memory_bytes",
                            "joint_planner"
                        ]
                        .iter()
                        .all(|k| v.get(k).is_none()),
                        "retained index owns its build, memory and traversal settings; conflicting index options"
                    );
                    let retained = self
                        .retained_indexes
                        .get_mut(handle)
                        .ok_or_else(|| anyhow::anyhow!("unknown retained index"))?;
                    ensure!(
                        retained.source == text(&v, "source")?,
                        "retained index belongs to another registered source"
                    );
                    ensure!(
                        retained.read_memory_bytes.saturating_add(128 * 1024 * 1024) <= available,
                        "insufficient session index query memory"
                    );
                    let forbidden: Vec<usize> = v
                        .get("forbidden_raw_tiles")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or_default();
                    retained.index.query_original(
                        source.as_ref(),
                        &v["geometry"],
                        text(&v, "crs")?,
                        &o,
                        &forbidden,
                        cancel,
                    )?
                } else if let Some(index) = v["index"].as_str() {
                    let index_path = if index.ends_with(".rsi") || index.contains("://") {
                        index.to_owned()
                    } else {
                        format!("{index}/summary.rsi")
                    };
                    let raw_path = if index.contains("://") {
                        String::new()
                    } else {
                        format!("{index}/pixels.rsr")
                    };
                    let forbidden: Vec<usize> = v
                        .get("forbidden_raw_tiles")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or_default();
                    let memory: usize = v
                        .get("read_memory_bytes")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or(32 * 1024 * 1024);
                    ensure!(
                        memory.saturating_add(128 * 1024 * 1024) <= self.available(),
                        "insufficient session index query memory"
                    );
                    crate::persistent::query_with_boundary_source(
                        crate::persistent::QueryOptions {
                            index_path: &index_path,
                            raw_path: &raw_path,
                            source_path: None,
                            expected_build_id: v["expected_build_id"].as_str(),
                            read_memory_bytes: memory,
                            summary_page_bytes: None,
                            coalesce_raw: true,
                            order_summaries: false,
                            forbidden_raw_tiles: &forbidden,
                            force_direct: false,
                            hierarchy: None,
                            joint_planner: v
                                .get("joint_planner")
                                .map(|x| serde_json::from_value(x.clone()))
                                .transpose()?,
                        },
                        &v["geometry"],
                        text(&v, "crs")?,
                        &o,
                        cancel,
                        Some(source.as_ref()),
                    )?
                } else {
                    ensure!(
                        self.available() >= 384 * 1024 * 1024,
                        "insufficient session direct query memory"
                    );
                    crate::streaming::measure_cached_source(
                        source.as_ref(),
                        &v["geometry"],
                        text(&v, "crs")?,
                        &o,
                        cancel,
                        &mut self.tile_cache,
                        0.,
                    )?
                };
                o.project(&mut result);
                result["source_access"] = source.diagnostics();
                result["provenance"] = decision.provenance;
                Ok(result)
            }
            "start_job" => {
                let id = text(&v, "id")?.to_owned();
                ensure!(
                    !self.jobs.contains_key(&id)
                        && !self.exactextract_jobs.contains_key(&id)
                        && self.jobs.len() + self.exactextract_jobs.len() < 8,
                    "job ID exists or job count budget exceeded"
                );
                let decision = crate::backend::resolve(
                    &v["job"],
                    v["job"]["slices"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|s| s.get("index").is_some())),
                )?;
                let mut specification = v["job"].clone();
                crate::backend::strip_fields(&mut specification);
                let spec: crate::batch::JobSpec = serde_json::from_value(specification)?;
                if decision.selected == crate::backend::Backend::Exactextract {
                    ensure!(
                        ["schedule", "geometry_layout", "window_policy", "tile_edge"]
                            .iter()
                            .all(|key| v["job"].get(key).is_none()),
                        "exactextract jobs do not accept native schedule/geometry_layout/window_policy/tile_edge selectors; use backend_options.strategy"
                    );
                    let job = crate::exactextract_job::Job::new(
                        spec,
                        decision,
                        serde_json::to_vec(&v["job"])?.len().saturating_mul(4),
                        self.available(),
                        v.get("checkpoint").is_some(),
                    )?;
                    let info = job.info();
                    self.exactextract_jobs.insert(id, job);
                    return Ok(info);
                }
                let checkpoint = v
                    .get("checkpoint")
                    .map(|c| serde_json::from_value(c.clone()))
                    .transpose()?;
                let job = crate::batch::Job::new(
                    spec,
                    checkpoint,
                    serde_json::to_vec(&v["job"])?.len().saturating_mul(4),
                    self.available(),
                )?;
                let mut result = job.info();
                result["provenance"] = decision.provenance.clone();
                self.job_provenance.insert(id.clone(), decision.provenance);
                self.jobs.insert(id, job);
                Ok(result)
            }
            "job_info" => {
                let id = text(&v, "id")?;
                if let Some(job) = self.exactextract_jobs.get(id) {
                    return Ok(job.info());
                }
                let mut info = self
                    .jobs
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown job"))?
                    .info();
                if let Some(p) = self.job_provenance.get(id) {
                    info["provenance"] = p.clone();
                }
                Ok(info)
            }
            "close_job" => {
                let id = text(&v, "id")?;
                let removed =
                    self.jobs.remove(id).is_some() | self.exactextract_jobs.remove(id).is_some();
                self.job_provenance.remove(id);
                ensure!(removed, "unknown job");
                Ok(json!({"closed":true}))
            }
            "next_job" => {
                let id = text(&v, "id")?.to_owned();
                let max_rows = v
                    .get("max_rows")
                    .map(|n| serde_json::from_value(n.clone()))
                    .transpose()?
                    .unwrap_or(128);
                let include_checkpoint: bool = v
                    .get("include_checkpoint")
                    .map(|n| serde_json::from_value(n.clone()))
                    .transpose()?
                    .unwrap_or(true);
                if let Some(mut job) = self.exactextract_jobs.remove(&id) {
                    let result = job.next(
                        max_rows,
                        |slice| {
                            if let Some(spec) = &slice.spec {
                                return crate::io::open_source(spec, cancel);
                            }
                            let source = slice.source.as_ref().expect("validated source");
                            if let Some(r) = self.sources.get(source) {
                                return Ok(Box::new(crate::batch::ResidentSource::new(r))
                                    as Box<dyn crate::source::WindowSource>);
                            }
                            if let Some(r) = self.readers.get(source) {
                                return Ok(Box::new(crate::batch::BorrowedSource(r.as_ref()))
                                    as Box<dyn crate::source::WindowSource>);
                            }
                            bail!("unknown batch source handle")
                        },
                        cancel,
                        &mut self.tile_cache,
                    );
                    self.exactextract_jobs.insert(id, job);
                    return result;
                }
                let mut job = self
                    .jobs
                    .remove(&id)
                    .ok_or_else(|| anyhow::anyhow!("unknown job"))?;
                let mut result = job.next_with_cache(
                    max_rows,
                    include_checkpoint,
                    |slice| {
                        if let Some(spec) = &slice.spec {
                            return crate::io::open_source(spec, cancel);
                        }
                        let id = slice.source.as_ref().expect("validated source reference");
                        if let Some(r) = self.sources.get(id) {
                            return Ok(Box::new(crate::batch::ResidentSource::new(r))
                                as Box<dyn crate::source::WindowSource>);
                        }
                        if let Some(r) = self.readers.get(id) {
                            return Ok(Box::new(crate::batch::BorrowedSource(r.as_ref()))
                                as Box<dyn crate::source::WindowSource>);
                        }
                        bail!("unknown batch source handle")
                    },
                    cancel,
                    &mut self.tile_cache,
                );
                if let (Ok(page), Some(p)) = (&mut result, self.job_provenance.get(&id)) {
                    page["provenance"] = p.clone();
                }
                self.jobs.insert(id, job);
                result
            }
            "prepare_file" => {
                ensure!(
                    self.available() >= 256 * 1024 * 1024,
                    "insufficient session budget for index builder"
                );
                let edge = v
                    .get("tile_edge")
                    .map(|x| serde_json::from_value(x.clone()))
                    .transpose()?
                    .unwrap_or(64);
                crate::persistent::build(
                    text(&v, "path")?,
                    text(&v, "index")?,
                    edge,
                    v["layout"].as_str().unwrap_or("band_major"),
                    v["summary_backend"].as_str().unwrap_or("flat"),
                    v.get("boundary_source")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or_default(),
                    cancel,
                )
            }
            "register_file" => {
                let id = text(&v, "id")?.to_owned();
                let path = text(&v, "path")?.to_owned();
                ensure!(
                    id.len() <= 1024 && path.len() <= 4096,
                    "registered source identity too long"
                );
                ensure!(
                    self.files.len() < 8,
                    "registered file count budget exceeded"
                );
                ensure!(
                    !self.files.contains_key(&id),
                    "registered file id exists; close it first"
                );
                ensure!(
                    !path.starts_with("/vsi") && !path.contains("://"),
                    "registered files require regular local paths"
                );
                ensure!(
                    self.available() >= 65_536,
                    "insufficient source metadata budget"
                );
                let signature = crate::io::registration_signature(&path)?;
                self.files.insert(
                    id.clone(),
                    RegisteredFile {
                        path,
                        signature,
                        source: None,
                    },
                );
                Ok(json!({"registered": id, "source_opened": false, "lazy": true}))
            }
            "close_file" => {
                let id = text(&v, "source")?;
                ensure!(self.files.remove(id).is_some(), "unknown registered source");
                self.tile_cache.clear();
                Ok(json!({"closed": id, "decoded_cache_cleared": true}))
            }
            "configure_file_cache" => {
                let bytes = v["bytes"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| anyhow::anyhow!("cache bytes must be a nonnegative integer"))?;
                ensure!(
                    bytes
                        <= self
                            .available()
                            .saturating_add(self.tile_cache.limit().max(self.tile_cache.bytes())),
                    "insufficient session cache budget"
                );
                self.tile_cache.set_limit(bytes)?;
                Ok(
                    json!({"cache_budget_bytes": bytes, "cache_resident_bytes": self.tile_cache.bytes()}),
                )
            }
            "clear_file_cache" => {
                self.tile_cache.clear();
                Ok(json!({"cleared": true, "cache_resident_bytes": 0}))
            }
            "measure_file" | "measure_registered_file" => {
                if v.get("index").is_some() {
                    ensure!(
                        op == "measure_file" && v.get("source").is_none(),
                        "indexed queries use measure_file with explicit index identity"
                    );
                    let index = text(&v, "index")?;
                    let remote = index.contains("://");
                    let index_path = if remote {
                        index.to_owned()
                    } else {
                        format!("{index}/summary.rsi")
                    };
                    let raw_path = if remote {
                        v["raw_path"].as_str().unwrap_or("").to_owned()
                    } else {
                        v["raw_path"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("{index}/pixels.rsr"))
                    };
                    let forbidden: Vec<usize> = v
                        .get("forbidden_raw_tiles")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or_default();
                    let memory: usize = v
                        .get("read_memory_bytes")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or(32 * 1024 * 1024);
                    ensure!(
                        (1024 * 1024..=128 * 1024 * 1024).contains(&memory)
                            && memory.saturating_add(128 * 1024 * 1024) <= self.available(),
                        "insufficient session memory for persistent query"
                    );
                    let backend = v["backend"].as_str().unwrap_or("auto");
                    ensure!(
                        ["auto", "persistent_flat", "persistent_hierarchy", "direct"]
                            .contains(&backend),
                        "unsupported persistent backend"
                    );
                    let o = options(&v)?;
                    return crate::persistent::query(
                        crate::persistent::QueryOptions {
                            joint_planner: v
                                .get("joint_planner")
                                .map(|x| serde_json::from_value(x.clone()))
                                .transpose()?,
                            index_path: &index_path,
                            raw_path: &raw_path,
                            source_path: v["path"].as_str(),
                            expected_build_id: v["expected_build_id"].as_str(),
                            read_memory_bytes: memory,
                            summary_page_bytes: v
                                .get("summary_page_bytes")
                                .map(|x| serde_json::from_value(x.clone()))
                                .transpose()?,
                            order_summaries: v
                                .get("order_summaries")
                                .map(|x| serde_json::from_value(x.clone()))
                                .transpose()?
                                .unwrap_or(false),
                            coalesce_raw: v
                                .get("coalesce_raw")
                                .map(|x| serde_json::from_value(x.clone()))
                                .transpose()?
                                .unwrap_or(true),
                            forbidden_raw_tiles: &forbidden,
                            force_direct: backend == "direct",
                            hierarchy: match backend {
                                "persistent_flat" => Some(false),
                                "persistent_hierarchy" => Some(true),
                                _ => None,
                            },
                        },
                        &v["geometry"],
                        text(&v, "crs")?,
                        &o,
                        cancel,
                    );
                }
                ensure!(
                    [
                        "raw_path",
                        "expected_build_id",
                        "read_memory_bytes",
                        "summary_page_bytes",
                        "coalesce_raw",
                        "order_summaries",
                        "joint_planner",
                        "forbidden_raw_tiles",
                        "backend"
                    ]
                    .iter()
                    .all(|k| v.get(k).is_none()),
                    "persistent options require index"
                );
                ensure!(
                    self.available() >= 384 * 1024 * 1024,
                    "insufficient session budget for bounded file execution"
                );
                let options = options(&v)?;
                if op == "measure_registered_file" {
                    ensure!(
                        v.get("path").is_none(),
                        "registered query does not accept a path"
                    );
                    let file = self
                        .files
                        .get_mut(text(&v, "source")?)
                        .ok_or_else(|| anyhow::anyhow!("unknown registered source"))?;
                    let locate_start = Instant::now();
                    let opened = file.source.is_none();
                    if opened {
                        let source = crate::io::LocalSource::open(&file.path)?;
                        ensure!(
                            source.metadata.source_id == file.signature,
                            "source changed since registration"
                        );
                        file.source = Some(source);
                    }
                    let open_ms = locate_start.elapsed().as_secs_f64() * 1000.;
                    let mut result = crate::streaming::measure_cached_source(
                        file.source.as_ref().expect("source opened"),
                        &v["geometry"],
                        text(&v, "crs")?,
                        &options,
                        cancel,
                        &mut self.tile_cache,
                        open_ms,
                    )?;
                    result["source_opened_this_request"] = json!(opened);
                    options.project(&mut result);
                    Ok(result)
                } else {
                    ensure!(
                        v.get("source").is_none(),
                        "path query does not accept a source id"
                    );
                    let mut result = crate::streaming::measure_local_file(
                        text(&v, "path")?,
                        &v["geometry"],
                        text(&v, "crs")?,
                        &options,
                        cancel,
                    )?;
                    options.project(&mut result);
                    Ok(result)
                }
            }
            "open" => {
                ensure!(self.sources.len() < 128, "source count budget exceeded");
                let id = text(&v, "id")?.to_string();
                ensure!(
                    !self.sources.contains_key(&id),
                    "source id already exists; close it first"
                );
                let available = self.available();
                ensure!(available >= 65_536, "insufficient source metadata budget");
                let r = if let Some(path) = v["path"].as_str() {
                    crate::io::open_local_cancellable(path, available - 65_536, cancel)?
                } else {
                    serde_json::from_value::<RasterInput>(v["raster"].clone())?
                        .decode_with_budget(available - 65_536, cancel)?
                };
                check_cancel(cancel)?;
                r.validate()?;
                ensure!(
                    self.bytes() + r.bytes() + 65_536 <= MAX_BYTES,
                    "session memory budget exceeded"
                );
                let result = json!({"id":id,"source_id":r.source_id,"grid":r.grid,"bands":r.bands.len(),"resident_bytes":r.bytes(),"open_ms":start.elapsed().as_secs_f64()*1000.});
                self.sources.insert(id, r);
                Ok(result)
            }
            "close" => {
                let id = text(&v, "source")?;
                ensure!(self.sources.remove(id).is_some(), "unknown source");
                self.indexes.remove(id);
                self.hierarchies.remove(id);
                self.cumulative.retain(|(source, _), _| source != id);
                Ok(json!({"closed":id}))
            }
            "stats" => Ok(
                json!({"sources":self.sources.len(),"plans":self.plans.len(),"indexes":self.indexes.len(),"resident_bytes":self.bytes(),"max_bytes":MAX_BYTES,"result_cache":false,
                "registered_files":self.files.len(),"registered_sources":self.readers.len(),
                "retained_indexes":self.retained_indexes.len(),"retained_index_metadata_bound_bytes":self.retained_indexes.len()*crate::persistent::PersistedIndex::RETAINED_BYTES,
                "decoded_cache_bytes":self.tile_cache.bytes(),"decoded_cache_limit":self.tile_cache.limit(),"decoded_cache_entries":self.tile_cache.len()}),
            ),
            "drop_plan" => {
                let id = text(&v, "plan")?;
                ensure!(self.plans.remove(id).is_some(), "unknown plan");
                Ok(json!({"dropped":id}))
            }
            "prepare" => {
                let id = text(&v, "source")?;
                let r = self
                    .sources
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown source"))?;
                let backend = v["backend"].as_str().unwrap_or("row_blocks");
                if backend == "hierarchy" {
                    let edge = v
                        .get("tile_edge")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or(16);
                    if let Some(ix) = self.hierarchies.get(id) {
                        ensure!(
                            ix.leaf_size == edge,
                            "hierarchy already prepared with different leaf size"
                        );
                        return Ok(
                            json!({"backend":backend,"already_prepared":true,"index_bytes":ix.bytes}),
                        );
                    }
                    ensure!(
                        crate::hierarchy::Hierarchy::estimate_bytes(r, edge)? + 65536
                            <= self.available(),
                        "session hierarchy memory budget exceeded"
                    );
                    let ix = crate::hierarchy::Hierarchy::build(r, edge, cancel)?;
                    let result = json!({"backend":backend,"index_bytes":ix.bytes,"preparation_ms":ix.preparation_ms,"leaf_size":edge,"persistent":false});
                    self.hierarchies.insert(id.to_owned(), ix);
                    return Ok(result);
                }
                if backend == "cumulative_full" || backend == "cumulative_blocked" {
                    let bands: Vec<usize> = v
                        .get("bands")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or_else(|| (0..r.bands.len()).collect());
                    let columns = v
                        .get("columns")
                        .map(|x| serde_json::from_value(x.clone()))
                        .transpose()?
                        .unwrap_or(64);
                    let origin = if backend == "cumulative_full" {
                        crate::cumulative::Origin::Full
                    } else {
                        crate::cumulative::Origin::Blocked { columns }
                    };
                    let key = (id.to_owned(), backend.to_owned());
                    if let Some(ix) = self.cumulative.get(&key) {
                        ensure!(
                            ix.band_ids == bands && ix.origin == origin,
                            "cumulative field already prepared with different bands or origin"
                        );
                        return Ok(
                            json!({"backend":backend,"already_prepared":true,"index_bytes":ix.bytes}),
                        );
                    }
                    // Conservative preallocation bound includes interval and validity
                    // fields, row origins, vector capacities and metadata.
                    let bound =
                        crate::cumulative::CumulativeField::estimate_bytes(r, &bands, origin)?
                            + 65536;
                    ensure!(
                        bound <= self.available(),
                        "session cumulative memory budget exceeded"
                    );
                    let ix = crate::cumulative::CumulativeField::build_selected(
                        r, &bands, origin, cancel,
                    )?;
                    let result = json!({"backend":backend,"index_bytes":ix.bytes,"preparation_ms":ix.preparation_ms,"bands":ix.band_ids,"persistent":false});
                    self.cumulative.insert(key, ix);
                    return Ok(result);
                }
                ensure!(backend == "row_blocks", "unsupported preparation backend");
                if let Some(ix) = self.indexes.get(id) {
                    return Ok(
                        json!({"source_id":ix.source_id,"index_bytes":ix.bytes,"already_prepared":true,"preparation_ms":0.,"persistent":false}),
                    );
                }
                ensure!(
                    Prepared::estimate_bytes(r)?.saturating_add(65_536) <= self.available(),
                    "session index memory budget exceeded"
                );
                let ix = Prepared::build(r, cancel)?;
                ensure!(
                    self.bytes() + ix.bytes + 65_536 <= MAX_BYTES,
                    "session index memory budget exceeded"
                );
                let result = json!({"source_id":r.source_id,"index_bytes":ix.bytes,"preparation_ms":start.elapsed().as_secs_f64()*1000.,"representation":"compensated_row_blocks_64","persistent":false});
                self.indexes.insert(id.to_string(), ix);
                Ok(result)
            }
            "compile" | "measure" => {
                let id = text(&v, "source")?;
                let r = self
                    .sources
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown source"))?;
                let mut policy_request = v.clone();
                if matches!(
                    v["backend"].as_str(),
                    Some(
                        "direct"
                            | "row_blocks"
                            | "hierarchy"
                            | "cumulative_full"
                            | "cumulative_blocked"
                    )
                ) {
                    policy_request["backend"] = json!("native");
                }
                let decision = crate::backend::resolve(&policy_request, false)?;
                if decision.selected == crate::backend::Backend::Exactextract {
                    ensure!(
                        op == "measure"
                            && ["plan", "strategy", "use_index", "mode"]
                                .iter()
                                .all(|k| v.get(k).is_none()),
                        "exactextract requires raw geometry and does not consume native plans/index/strategy/mode"
                    );
                    let o = options(&v)?;
                    let reader = crate::batch::ResidentSource::new(r);
                    let input = crate::exactextract::Input {
                        source: &reader,
                        bands: o.selected_bands(r.bands.len()),
                    };
                    let available = self.available();
                    let output = crate::exactextract::execute(
                        &[input],
                        std::slice::from_ref(&v["geometry"]),
                        text(&v, "crs")?,
                        &o,
                        &decision.options,
                        cancel,
                        &mut self.tile_cache,
                        available,
                    )?;
                    let stats = crate::exactextract::statistics(&o)?;
                    return Ok(
                        json!({"bands":output.bands(0,0,&stats),"statistics":stats,"grid":r.grid,"source_id":r.source_id,"mode":"exactextract_fractional","provenance":decision.provenance,"work":output.metrics}),
                    );
                }
                let backend = match v["backend"].as_str().unwrap_or("auto") {
                    "native" => "auto",
                    other => other,
                };
                ensure!(
                    [
                        "auto",
                        "direct",
                        "row_blocks",
                        "hierarchy",
                        "cumulative_full",
                        "cumulative_blocked"
                    ]
                    .contains(&backend),
                    "unsupported execution backend"
                );
                let mut fallback_reason = None;
                if op == "measure" && backend == "hierarchy" {
                    ensure!(
                        v.get("strategy").is_none() && v.get("use_index").is_none(),
                        "hierarchy backend does not accept scanline strategy/use_index options"
                    );
                    ensure!(
                        self.available() >= 128 * 1024 * 1024,
                        "insufficient session scratch for hierarchy query"
                    );
                    ensure!(
                        v.get("plan").is_none(),
                        "hierarchy queries take geometry, not a full-resolution plan"
                    );
                    let o = options(&v)?;
                    o.validate(r.bands.len())?;
                    let ix = self
                        .hierarchies
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("hierarchy not prepared"))?;
                    let answer = ix.query(r, &v["geometry"], text(&v, "crs")?, &o, cancel)?;
                    let mut result = json!({"bands":answer.bands,"grid":r.grid,"source_id":r.source_id,"mode":"native_grid_planar","precision":"f64_compensated","strategy":"native_hierarchy","work":answer.diagnostics,"plan_reused":false,"selection_reason":"forced prepared native hierarchy","timing_ms":{"total":start.elapsed().as_secs_f64()*1000.}});
                    o.project(&mut result);
                    return Ok(result);
                }
                if op == "measure" && backend.starts_with("cumulative_") {
                    ensure!(
                        v.get("strategy").is_none() && v.get("use_index").is_none(),
                        "cumulative backend does not accept scanline strategy/use_index options"
                    );
                    ensure!(
                        self.available() >= 128 * 1024 * 1024,
                        "insufficient session scratch for cumulative query"
                    );
                    let o = options(&v)?;
                    o.validate(r.bands.len())?;
                    let supported = o.statistics.as_ref().is_some_and(|s| {
                        s.iter()
                            .all(|s| ["sum", "support", "mean"].contains(&s.as_str()))
                    }) && v.get("plan").is_none();
                    if supported {
                        ensure!(
                            text(&v, "crs")? == r.grid.crs,
                            "query CRS must exactly match source CRS"
                        );
                        let ix = self
                            .cumulative
                            .get(&(id.to_owned(), backend.to_owned()))
                            .ok_or_else(|| anyhow::anyhow!("cumulative field not prepared"))?;
                        let answer = ix.query_selected(
                            r,
                            &v["geometry"],
                            &o.selected_bands(r.bands.len()),
                            o.needs_sum(),
                            cancel,
                        )?;
                        let mut result = json!({"bands":answer.bands,"grid":r.grid,"source_id":r.source_id,"mode":"native_grid_planar","precision":"f64_interval_screened_compensated_fallback","strategy":answer.diagnostics.strategy,"work":answer.diagnostics,"plan_reused":false,"selection_reason":"forced cumulative additive field; error screen selects strict fallback when required","timing_ms":{"total":start.elapsed().as_secs_f64()*1000.}});
                        o.project(&mut result);
                        result["reduction_dependencies"]["count"] = json!(false);
                        return Ok(result);
                    }
                    fallback_reason = Some(
                        "requested count/extrema/histogram/weights or reused plan requires scanline reductions",
                    );
                }
                let owned;
                let reused = v["plan"].as_str().is_some();
                let p = if let Some(id) = v["plan"].as_str() {
                    self.plans
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("unknown plan"))?
                } else {
                    owned = compile_with_budget(
                        &r.grid,
                        &v["geometry"],
                        text(&v, "crs")?,
                        v["strategy"].as_str().unwrap_or("scanline"),
                        cancel,
                        self.available(),
                    )?;
                    &owned
                };
                ensure!(p.grid == r.grid, "coverage plan grid mismatch");
                if v["op"] == "compile" {
                    ensure!(self.plans.len() < 128, "plan count budget exceeded");
                    let id = text(&v, "id")?.to_string();
                    ensure!(!self.plans.contains_key(&id), "plan id already exists");
                    ensure!(
                        p.bytes().saturating_add(65_536) <= self.available(),
                        "session plan memory budget exceeded"
                    );
                    let mut result = p.diagnostics();
                    if v["debug_cells"] == true {
                        result["cells"] = serde_json::to_value(p.debug_cells()?)?;
                    }
                    result["timing_ms"] = json!({"validation":p.validation_ms,"compilation":p.compilation_ms,"total":start.elapsed().as_secs_f64()*1000.});
                    self.plans.insert(id, p.clone());
                    Ok(result)
                } else {
                    let options = options(&v)?;
                    ensure!(
                        backend != "row_blocks"
                            || (self.indexes.contains_key(id) && v["use_index"] != false),
                        "forced row_blocks requires a prepared index and use_index enabled"
                    );
                    let ix = if v["use_index"] == false
                        || backend == "direct"
                        || !options.summaries_eligible()
                    {
                        None
                    } else {
                        self.indexes.get(id)
                    };
                    let agg = Instant::now();
                    let bands = measure(r, p, &options, ix, cancel)?;
                    let agg_ms = agg.elapsed().as_secs_f64() * 1000.;
                    let strategy =
                        if p.strategy == "scanline" && ix.is_some() && options.summaries_eligible()
                        {
                            "scanline_blocked"
                        } else {
                            &p.strategy
                        };
                    let mut result = json!({"bands":bands,"grid":r.grid,"source_id":r.source_id,"mode":"native_grid_planar","precision":"f64_compensated","strategy":strategy,"plan":p.diagnostics(),"plan_reused":reused,"timing_ms":{"validation":if reused{0.}else{p.validation_ms},"compilation":if reused{0.}else{p.compilation_ms},"aggregation":agg_ms,"total":start.elapsed().as_secs_f64()*1000.}});
                    options.project(&mut result);
                    result["selection_reason"] =
                        json!(fallback_reason.unwrap_or(if ix.is_some() {
                            "available compensated row blocks"
                        } else {
                            "direct raw reduction; no eligible prepared index"
                        }));
                    result["backend_requested"] = json!(backend);
                    result["provenance"] = decision.provenance;
                    Ok(result)
                }
            }
            _ => bail!("unknown operation"),
        }
    }
}

pub fn request(session: &mut Session, input: &str, cancel: &AtomicBool) -> String {
    let start = Instant::now();
    let reserve = input.len().saturating_mul(64);
    let mut parse_ms = 0.;
    let mut execution_ms = 0.;
    let response = if input.len() > 64 * 1024 * 1024 {
        Err(anyhow::anyhow!("request exceeds 64 MiB"))
    } else if reserve > MAX_BYTES.saturating_sub(session.bytes()) {
        Err(anyhow::anyhow!("request allocation budget exceeded"))
    } else {
        session.transient_reserve = reserve;
        let parse_start = Instant::now();
        let parsed = serde_json::from_str(input).map_err(anyhow::Error::from);
        parse_ms = parse_start.elapsed().as_secs_f64() * 1000.;
        let execution_start = Instant::now();
        let result = parsed.and_then(|v| session.call(v, cancel));
        execution_ms = execution_start.elapsed().as_secs_f64() * 1000.;
        result
    };
    session.transient_reserve = 0;
    let envelope_start = Instant::now();
    let mut envelope = match response {
        Ok(v) => {
            let mut envelope = json!({"ok":true});
            envelope["result"] = v;
            envelope
        }
        Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
    };
    envelope["request_ms"] = json!(start.elapsed().as_secs_f64() * 1000.);
    let envelope_ms = envelope_start.elapsed().as_secs_f64() * 1000.;
    let serialization_start = Instant::now();
    let mut encoded = serde_json::to_string(&envelope)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialization failed\"}".to_string());
    let serialization_ms = serialization_start.elapsed().as_secs_f64() * 1000.;
    let drop_start = Instant::now();
    drop(envelope);
    let response_drop_ms = drop_start.elapsed().as_secs_f64() * 1000.;
    // Append the small timing descriptor after serializing the result once. Its
    // own encoding/FFI allocation remains in the binding's native-call residual.
    let timing = json!({"parse_ms":parse_ms,"execution_ms":execution_ms,
        "envelope_ms":envelope_ms,"serialization_ms":serialization_ms,"response_drop_ms":response_drop_ms,
        "before_timing_append_ms":start.elapsed().as_secs_f64()*1000.});
    encoded.pop();
    encoded.push_str(",\"native_timing\":");
    encoded.push_str(&timing.to_string());
    encoded.push('}');
    encoded
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn oversized_cache_request_rejects_without_admission_overflow() {
        let mut session = Session::default();
        let error = session
            .call(
                json!({"op":"register_source","id":"oversized","spec":{
                "location":"not-opened.tif","http":{"cache_bytes":usize::MAX}}}),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("insufficient session reader memory")
        );
        assert!(session.readers.is_empty());
    }

    #[test]
    fn actual_reader_capacity_is_charged_before_admitting_another_reader() {
        struct Reserved(usize);
        impl crate::source::WindowSource for Reserved {
            fn metadata(&self) -> &crate::source::RasterMetadata {
                panic!("metadata not needed for capacity accounting")
            }
            fn verify_immutable(&self) -> Result<()> {
                Ok(())
            }
            fn retained_memory_bound(&self) -> usize {
                self.0
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
            ) -> Result<(Raster, crate::source::ReadMetrics)> {
                bail!("not a readable fixture")
            }
        }
        let mut session = Session::default();
        let baseline = session.bytes();
        session
            .readers
            .insert("small".into(), Box::new(Reserved(1)));
        session
            .readers
            .insert("large".into(), Box::new(Reserved(40 << 20)));
        assert_eq!(session.bytes() - baseline, 56 << 20);
        session.transient_reserve = MAX_BYTES - baseline - (80 << 20);
        let error = session
            .call(
                json!({"op":"register_source","id":"new","spec":{"location":"not-opened.tif"}}),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("insufficient session reader memory")
        );
        assert_eq!(session.readers.len(), 2);
    }

    #[test]
    fn future_decoded_cache_growth_is_reserved_before_touching_a_file() {
        let mut session = Session::default();
        let cancel = AtomicBool::new(false);
        session
            .call(
                json!({"op":"configure_file_cache","bytes":128 * 1024 * 1024}),
                &cancel,
            )
            .unwrap();
        // Model available memory consumed by other active native request state,
        // without allocating gigabytes in a unit test.
        session.transient_reserve = MAX_BYTES - 448 * 1024 * 1024;
        let query = json!({"op":"measure_registered_file","source":"missing","geometry":{},"crs":"EPSG:3857"});
        assert!(
            session
                .call(query.clone(), &cancel)
                .unwrap_err()
                .to_string()
                .contains("insufficient session budget")
        );
        session
            .call(
                json!({"op":"configure_file_cache","bytes":64 * 1024 * 1024}),
                &cancel,
            )
            .unwrap();
        assert!(
            session
                .call(query, &cancel)
                .unwrap_err()
                .to_string()
                .contains("unknown registered source")
        );
    }

    #[test]
    fn retained_identifiers_crs_and_units_are_bounded_and_accounted() {
        let mut session = Session::default();
        let cancel = AtomicBool::new(false);
        let input = json!({"op":"open","id":"r","raster":{"grid":{"width":1,"height":1,"transform":[0,1,0,1,0,-1],"crs":"LOCAL"},"bands":[{"values":[1],"unit":"units"}]}});
        for field in ["id", "unit", "crs"] {
            let mut oversized = input.clone();
            match field {
                "id" => oversized["id"] = json!("x".repeat(1025)),
                "unit" => oversized["raster"]["bands"][0]["unit"] = json!("x".repeat(1025)),
                _ => oversized["raster"]["grid"]["crs"] = json!("x".repeat(4097)),
            }
            assert!(session.call(oversized, &cancel).is_err());
            assert_eq!(session.bytes(), 0);
        }
        session.call(input, &cancel).unwrap();
        assert!(session.bytes() > 65_536 + 9);
        let before = session.bytes();
        let oversized_plan = json!({"op":"compile","id":"x".repeat(1025),"source":"r","crs":"LOCAL","geometry":{"type":"Polygon","coordinates":[]}});
        assert!(
            session
                .call(oversized_plan, &cancel)
                .unwrap_err()
                .to_string()
                .contains("exceeds 1024 bytes")
        );
        assert_eq!(session.bytes(), before);
    }
}
