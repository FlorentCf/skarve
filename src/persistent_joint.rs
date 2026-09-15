//! Execution adapter for the opt-in bounded joint planner.
//! Physical normalized pages are known; compressed original sources are excluded.
use super::*;
use crate::joint_plan::{self as jp, Action, ChoiceKind, Fallback, Forest, Node, ReadSpan};

fn default_mode() -> jp::Mode {
    jp::Mode::Joint
}
fn default_range() -> u64 {
    65536
}
fn default_summary_cpu() -> u64 {
    50
}
fn default_raw_cpu() -> u64 {
    5
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointQueryOptions {
    #[serde(default = "default_mode")]
    pub mode: jp::Mode,
    /// Supplied serial transport cost model; no network calibration is inferred.
    pub model: jp::CostModel,
    #[serde(default)]
    pub limits: jp::Limits,
    #[serde(default = "default_range")]
    pub max_range_bytes: u64,
    #[serde(default = "default_range")]
    pub max_summary_gap_bytes: u64,
    /// Cost per selected band and summary contribution.
    #[serde(default = "default_summary_cpu")]
    pub summary_reduction_ns: u64,
    /// Cost per positive cell and selected band (boundary geometry is separate).
    #[serde(default = "default_raw_cpu")]
    pub raw_cell_reduction_ns: u64,
}
impl JointQueryOptions {
    pub(super) fn reserve(self) -> Result<usize> {
        ensure!(
            (1..=RANGE as u64).contains(&self.max_range_bytes),
            "joint range must be 1 byte..4 MiB"
        );
        ensure!(
            (65536..=32 * 1024 * 1024).contains(&self.limits.max_planner_bytes),
            "joint planner reservation must be 64 KiB..32 MiB"
        );
        ensure!(
            self.max_summary_gap_bytes <= MAX_QUERY_BYTES,
            "joint summary gap exceeds IO budget"
        );
        // The planner reservation includes its forest and result. Separate live
        // task metadata, JSON diagnostics and two range caches are reserved here.
        Ok(self.limits.max_planner_bytes + 2 * self.max_range_bytes as usize + 8 * 1024 * 1024)
    }
}
#[derive(Clone, Copy)]
struct Task {
    level: usize,
    tx: usize,
    ty: usize,
    kind: ChoiceKind,
}
pub(super) struct Execution {
    pub plan: jp::Plan,
    tasks: Vec<Task>,
    caches: [Vec<u8>; 2],
    cache_offsets: [u64; 2],
    loaded: Vec<bool>,
    pub candidates: usize,
    pub boundary_work: usize,
    pub planning_ms: f64,
    pub forest_nodes: usize,
}
impl Execution {
    pub fn next(&mut self) -> Option<(usize, usize, usize, Relation)> {
        self.tasks.pop().map(|t| {
            (
                t.level,
                t.tx,
                t.ty,
                if t.kind == ChoiceKind::Summary {
                    Relation::Inside
                } else {
                    Relation::Boundary
                },
            )
        })
    }
    pub fn read(
        &mut self,
        source: &mut Source,
        trace: &mut Trace,
        source_id: usize,
        offset: u64,
        length: usize,
        tile: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(length as u64)
            .context("joint read overflow")?;
        let cache_end = self.cache_offsets[source_id] + self.caches[source_id].len() as u64;
        if self.caches[source_id].is_empty()
            || offset < self.cache_offsets[source_id]
            || end > cache_end
        {
            let (i, range) = self
                .plan
                .ranges
                .iter()
                .copied()
                .enumerate()
                .find(|(_, r)| {
                    r.source == source_id as u32 && r.offset <= offset && r.offset + r.len >= end
                })
                .context("joint execution demand absent from plan")?;
            ensure!(
                !self.loaded[i],
                "joint execution revisited a discarded range"
            );
            drop(std::mem::take(&mut self.caches[source_id]));
            self.caches[source_id] = trace.read(
                source,
                if source_id == 0 { "index" } else { "raw" },
                range.offset,
                range.len as usize,
                Some(tile),
                cancel,
            )?;
            self.cache_offsets[source_id] = range.offset;
            self.loaded[i] = true;
        }
        let at = (offset - self.cache_offsets[source_id]) as usize;
        Ok(self.caches[source_id][at..at + length].to_vec())
    }
    pub fn finish(&self, trace: &Trace, initial_reads: usize, initial_bytes: u64) -> Result<()> {
        ensure!(
            self.loaded.iter().all(|v| *v),
            "joint execution left a planned range unread"
        );
        ensure!(
            trace.records.len() - initial_reads == self.plan.cost.requests
                && trace.index_bytes + trace.raw_bytes - initial_bytes
                    == self.plan.cost.fetched_bytes,
            "joint planned and executed IO differ"
        );
        Ok(())
    }
    pub fn diagnostics(&self) -> Value {
        json!({"mode":self.plan.mode,"exact":self.plan.exact,"model_scope":"serial additive request latency, fetched bytes, supplied reduction CPU; excludes geometry/search/decoding CPU", "cost":self.plan.cost,"search":self.plan.diagnostics,"forest_nodes":self.forest_nodes,"planning_ms":self.planning_ms,"ranges":self.plan.ranges,"selected_actions":self.plan.choices.len(),"summary_actions":self.plan.choices.iter().filter(|c|c.kind==ChoiceKind::Summary).count(),"raw_actions":self.plan.choices.iter().filter(|c|c.kind==ChoiceKind::Raw).count(),"range_cache_bytes":self.caches.iter().map(Vec::len).sum::<usize>(),"original_encoded_source_supported":false,"raw_interior_alternatives":false})
    }
}
struct Builder<'a> {
    h: &'a Header,
    predicate: &'a GeometryPredicate,
    config: JointQueryOptions,
    selected_bands: usize,
    groups: Vec<usize>,
    cancel: &'a AtomicBool,
    forest: Forest,
    metadata: Vec<(usize, usize, usize)>,
    levels: Vec<(usize, usize, usize)>,
    candidates: usize,
    boundary_work: usize,
    forbidden: &'a [usize],
    spans: usize,
}
impl Builder<'_> {
    fn node(
        &mut self,
        level: usize,
        tx: usize,
        ty: usize,
        inherited_inside: bool,
    ) -> Result<Option<usize>> {
        check_cancel(self.cancel)?;
        let (nx, _, offset) = self.levels[level];
        let record = offset + ty * nx + tx;
        let edge = self.h.tile_edge << level;
        let bounds = [
            tx * edge,
            ty * edge,
            ((tx + 1) * edge).min(self.h.grid.width),
            ((ty + 1) * edge).min(self.h.grid.height),
        ];
        let relation = if inherited_inside {
            Relation::Inside
        } else {
            self.candidates += 1;
            self.predicate.classify(bounds)
        };
        ensure!(
            self.candidates.saturating_mul(self.predicate.vertices()) <= 100_000_000,
            "joint predicate work budget exceeded"
        );
        if relation == Relation::Outside {
            return Ok(None);
        }
        ensure!(
            self.forest.nodes.len() < jp::MAX_NODES,
            "joint forest exceeds 4096 nodes"
        );
        let inside = relation == Relation::Inside;
        let summary = if inside {
            self.spans += 1;
            Some(Action {
                reads: vec![ReadSpan {
                    source: 0,
                    offset: HEADER as u64 + (record * self.h.summary_size()) as u64,
                    len: self.h.summary_size() as u64,
                }],
                reduction_ns: self
                    .config
                    .summary_reduction_ns
                    .checked_mul(self.selected_bands as u64)
                    .context("joint summary CPU overflow")?,
            })
        } else {
            None
        };
        let index = self.forest.nodes.len();
        self.forest.nodes.push(Node {
            id: record as u64,
            certified: inside,
            summary,
            fallback: Fallback::Unavailable,
        });
        self.metadata.push((level, tx, ty));
        if level > 0 {
            let (cx, cy, _) = self.levels[level - 1];
            let mut children = Vec::with_capacity(4);
            for yy in ty * 2..(ty * 2 + 2).min(cy) {
                for xx in tx * 2..(tx * 2 + 2).min(cx) {
                    if let Some(c) = self.node(level - 1, xx, yy, inside)? {
                        children.push(c)
                    }
                }
            }
            if !children.is_empty() {
                self.forest.nodes[index].fallback = Fallback::Children(children)
            }
        } else if !inside {
            let count = (bounds[2] - bounds[0]) * (bounds[3] - bounds[1]);
            self.boundary_work = self
                .boundary_work
                .saturating_add(count.saturating_mul(self.predicate.vertices()));
            ensure!(
                self.boundary_work <= 100_000_000,
                "joint boundary preflight work budget exceeded"
            );
            let cells = self.predicate.boundary_cells(bounds, self.cancel)?;
            // This leaf is last in DFS, so a zero-area contact can be removed
            // without shifting any already stored child indices.
            if cells.is_empty() {
                self.forest.nodes.pop();
                self.metadata.pop();
                return Ok(None);
            }
            ensure!(
                !self.forbidden.contains(&record),
                "forbidden raw interior tile read: {record}"
            );
            ensure!(
                self.h.raw_size() as u64 <= self.config.max_range_bytes,
                "joint indivisible normalized raw page exceeds range cap"
            );
            let reads = self
                .groups
                .iter()
                .map(|&g| ReadSpan {
                    source: 1,
                    offset: HEADER as u64
                        + ((record * self.h.groups() + g) * self.h.raw_size()) as u64,
                    len: self.h.raw_size() as u64,
                })
                .collect::<Vec<_>>();
            self.spans += reads.len();
            self.forest.nodes[index].fallback = Fallback::Raw(Action {
                reads,
                reduction_ns: self
                    .config
                    .raw_cell_reduction_ns
                    .checked_mul(cells.len() as u64)
                    .and_then(|v| v.checked_mul(self.selected_bands as u64))
                    .context("joint raw CPU overflow")?,
            });
        }
        ensure!(
            self.spans <= jp::MAX_SPANS,
            "joint forest exceeds 4096 physical demands"
        );
        // Boundary parents with no positive descendants have no contribution.
        if !inside && matches!(self.forest.nodes[index].fallback, Fallback::Unavailable) {
            ensure!(
                index + 1 == self.forest.nodes.len(),
                "joint empty-subtree compaction invariant"
            );
            self.forest.nodes.pop();
            self.metadata.pop();
            return Ok(None);
        }
        Ok(Some(index))
    }
}
pub(super) fn build(
    h: &Header,
    predicate: &GeometryPredicate,
    config: JointQueryOptions,
    read_bands: &[usize],
    selected_bands: usize,
    forbidden: &[usize],
    trace: &Trace,
    cancel: &AtomicBool,
) -> Result<Execution> {
    let started = Instant::now();
    let mut groups = read_bands
        .iter()
        .map(|b| b / h.band_group)
        .collect::<Vec<_>>();
    groups.sort_unstable();
    groups.dedup();
    let capacity = h.summary_records().min(jp::MAX_NODES);
    ensure!(
        capacity.saturating_mul(512).saturating_add(16384) <= config.limits.max_planner_bytes,
        "joint forest construction exceeds planner reservation"
    );
    let mut b = Builder {
        h,
        predicate,
        config,
        selected_bands,
        groups,
        cancel,
        forest: Forest {
            nodes: Vec::with_capacity(capacity),
            roots: Vec::new(),
        },
        metadata: Vec::with_capacity(capacity),
        levels: h.levels(),
        candidates: 0,
        boundary_work: 0,
        forbidden,
        spans: 0,
    };
    if b.levels.len() == 1 {
        let [x0, y0, x1, y1] = predicate.bounds();
        for ty in y0 / h.tile_edge..y1.div_ceil(h.tile_edge) {
            for tx in x0 / h.tile_edge..x1.div_ceil(h.tile_edge) {
                if let Some(n) = b.node(0, tx, ty, false)? {
                    b.forest.roots.push(n)
                }
            }
        }
    } else if let Some(n) = b.node(b.levels.len() - 1, 0, 0, false)? {
        b.forest.roots.push(n)
    }
    let policies = [
        jp::SourcePolicy {
            source: 0,
            max_range_bytes: config.max_range_bytes,
            max_gap_bytes: config.max_summary_gap_bytes,
            strict_no_overread: false,
        },
        jp::SourcePolicy {
            source: 1,
            max_range_bytes: config.max_range_bytes,
            max_gap_bytes: 0,
            strict_no_overread: true,
        },
    ];
    let mut limits = config.limits;
    limits.max_requests = limits.max_requests.min(4096 - trace.records.len());
    limits.max_total_read_bytes = limits
        .max_total_read_bytes
        .min(MAX_QUERY_BYTES - trace.index_bytes - trace.raw_bytes);
    ensure!(
        jp::buffer_bound(&b.forest, limits)? <= config.limits.max_planner_bytes,
        "joint forest exceeds planner memory reservation"
    );
    let plan = jp::plan(
        &b.forest,
        &policies,
        config.model,
        limits,
        config.mode,
        cancel,
    )?;
    let mut tasks = plan
        .choices
        .iter()
        .map(|choice| {
            let (level, tx, ty) = b.metadata[choice.node];
            Task {
                level,
                tx,
                ty,
                kind: choice.kind,
            }
        })
        .collect::<Vec<_>>();
    tasks.sort_unstable_by_key(|t| {
        std::cmp::Reverse((
            usize::from(t.kind == ChoiceKind::Raw),
            b.levels[t.level].2 + t.ty * b.levels[t.level].0 + t.tx,
        ))
    });
    let loaded = vec![false; plan.ranges.len()];
    Ok(Execution {
        plan,
        tasks,
        caches: [Vec::new(), Vec::new()],
        cache_offsets: [0, 0],
        loaded,
        candidates: b.candidates,
        boundary_work: b.boundary_work,
        planning_ms: started.elapsed().as_secs_f64() * 1000.,
        forest_nodes: b.forest.nodes.len(),
    })
}
