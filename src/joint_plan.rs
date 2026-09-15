//! Bounded joint selection of certified tree covers and physical read ranges.
//!
//! Exactness is only for the supplied forest, indivisible requested spans, and
//! serial additive latency/byte/CPU model. This is enumeration plus interval DP,
//! not a claim of a new general optimization principle or universal optimality.
use crate::model::check_cancel;
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, sync::atomic::AtomicBool};

pub const MAX_NODES: usize = 4096;
pub const MAX_SPANS: usize = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadSpan {
    pub source: u32,
    pub offset: u64,
    pub len: u64,
}
impl ReadSpan {
    fn end(self) -> u64 {
        self.offset + self.len
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Action {
    pub reads: Vec<ReadSpan>,
    pub reduction_ns: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Fallback {
    Children(Vec<usize>),
    Raw(Action),
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: u64,
    pub certified: bool,
    pub summary: Option<Action>,
    pub fallback: Fallback,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Forest {
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SourcePolicy {
    pub source: u32,
    pub max_range_bytes: u64,
    pub max_gap_bytes: u64,
    /// Forbids bytes outside requested spans, not repeated bytes where two
    /// overlapping indivisible demands cannot fit in one range.
    pub strict_no_overread: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostModel {
    pub request_latency_ns: u64,
    pub bandwidth_bytes_per_second: u64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_states: usize,
    pub beam_width: usize,
    pub max_planner_bytes: usize,
    pub max_total_read_bytes: u64,
    pub max_requests: usize,
    pub max_range_transitions: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_states: 4096,
            beam_width: 16,
            max_planner_bytes: 8 * 1024 * 1024,
            max_total_read_bytes: 128 * 1024 * 1024,
            max_requests: 4096,
            max_range_transitions: 50_000_000,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Joint,
    Greedy,
    FixedRequestFirst,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceKind {
    Summary,
    Raw,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Choice {
    pub node: usize,
    pub kind: ChoiceKind,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Cost {
    pub requests: usize,
    pub fetched_bytes: u64,
    pub unique_requested_bytes: u64,
    pub reduction_ns: u64,
    /// Exact objective numerator; denominator is bandwidth_bytes_per_second.
    #[serde(with = "decimal_u128")]
    pub score_numerator: u128,
    pub estimated_ns: f64,
}
impl Cost {
    fn key(self) -> (u128, usize, u64, u64) {
        (
            self.score_numerator,
            self.requests,
            self.fetched_bytes,
            self.reduction_ns,
        )
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub covers_evaluated: usize,
    pub range_transitions: u64,
    pub beam_rounds: usize,
    pub cover_count_capped: usize,
    pub planner_buffer_bound: usize,
    pub state_budget_exhausted: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub choices: Vec<Choice>,
    pub ranges: Vec<ReadSpan>,
    pub cost: Cost,
    pub exact: bool,
    pub mode: Mode,
    pub diagnostics: Diagnostics,
}
struct Validated {
    counts: Vec<usize>,
    fallback_counts: Vec<usize>,
    postorder: Vec<usize>,
    ranks: Vec<usize>,
    spans: usize,
}
struct Work<'a> {
    diagnostics: Diagnostics,
    limits: Limits,
    cancel: &'a AtomicBool,
}
impl Work<'_> {
    fn transition(&mut self) -> Result<()> {
        ensure!(
            self.diagnostics.range_transitions < self.limits.max_range_transitions,
            "joint planner work bound exceeded"
        );
        self.diagnostics.range_transitions += 1;
        if self.diagnostics.range_transitions % 64 == 0 {
            check_cancel(self.cancel)?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct RangeState {
    score: u128,
    bytes: u64,
    requests: usize,
    previous: usize,
}
#[derive(Clone, Copy)]
struct ConstrainedState {
    bytes: u64,
    previous: usize,
}
struct Evaluation {
    cost: Cost,
    ranges: Vec<ReadSpan>,
}
#[derive(Clone)]
struct Candidate {
    choices: Vec<Choice>,
    last_rank: Option<usize>,
    cost: Cost,
}

fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| anyhow!("joint planner size overflow"))
}
fn mul(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| anyhow!("joint planner size overflow"))
}
fn policy(policies: &[SourcePolicy], id: u32) -> Result<SourcePolicy> {
    policies
        .iter()
        .copied()
        .find(|p| p.source == id)
        .ok_or_else(|| anyhow!("unknown joint planner source id"))
}
fn action<'a>(forest: &'a Forest, c: Choice) -> &'a Action {
    match c.kind {
        ChoiceKind::Summary => forest.nodes[c.node].summary.as_ref().unwrap(),
        ChoiceKind::Raw => match &forest.nodes[c.node].fallback {
            Fallback::Raw(a) => a,
            _ => unreachable!(),
        },
    }
}
/// Conservative live buffer reservation, including caller-owned forest capacities.
/// Decoder/transport buffers and serialized diagnostics remain caller obligations.
pub fn buffer_bound(forest: &Forest, limits: Limits) -> Result<usize> {
    let n = forest.nodes.len();
    let mut spans = 0usize;
    let mut bytes = add(
        mul(forest.nodes.capacity(), size_of::<Node>())?,
        mul(forest.roots.capacity(), size_of::<usize>())?,
    )?;
    for node in &forest.nodes {
        if let Some(a) = &node.summary {
            spans = add(spans, a.reads.len())?;
            bytes = add(bytes, mul(a.reads.capacity(), size_of::<ReadSpan>())?)?;
        }
        match &node.fallback {
            Fallback::Raw(a) => {
                spans = add(spans, a.reads.len())?;
                bytes = add(bytes, mul(a.reads.capacity(), size_of::<ReadSpan>())?)?;
            }
            Fallback::Children(c) => bytes = add(bytes, mul(c.capacity(), size_of::<usize>())?)?,
            _ => {}
        }
    }
    // Validation/order/count stacks, traversal/branch bits, bounded beam populations,
    // best/temporary covers, input spans, best and temporary ranges, range DP states.
    bytes = add(bytes, mul(n, 128)?)?;
    bytes = add(
        bytes,
        mul(
            mul(n, add(mul(limits.beam_width, 2)?, 8)?)?,
            size_of::<Choice>(),
        )?,
    )?;
    bytes = add(
        bytes,
        mul(add(mul(limits.beam_width, 2)?, 8)?, size_of::<Candidate>())?,
    )?;
    bytes = add(bytes, mul(spans, 4 * size_of::<ReadSpan>())?)?;
    bytes = add(bytes, mul(add(spans, 1)?, size_of::<RangeState>())?)?;
    let constrained = add(spans.min(64), 1)?;
    add(
        bytes,
        mul(
            mul(constrained, constrained)?,
            size_of::<ConstrainedState>(),
        )?,
    )
}
fn validate(
    f: &Forest,
    policies: &[SourcePolicy],
    model: CostModel,
    limits: Limits,
) -> Result<Validated> {
    ensure!(
        f.nodes.len() <= MAX_NODES && f.roots.len() <= f.nodes.len(),
        "joint planner node bound exceeded"
    );
    ensure!(
        limits.max_states > 0
            && limits.max_states <= 65_536
            && limits.beam_width > 0
            && limits.beam_width <= 128,
        "invalid joint search bounds"
    );
    ensure!(
        limits.max_requests > 0
            && limits.max_requests <= 4096
            && limits.max_total_read_bytes <= 128 * 1024 * 1024,
        "invalid joint I/O bounds"
    );
    ensure!(
        model.bandwidth_bytes_per_second > 0,
        "joint bandwidth must be positive"
    );
    ensure!(policies.len() <= 16, "too many joint sources");
    for (i, p) in policies.iter().enumerate() {
        ensure!(
            p.max_range_bytes > 0 && p.max_range_bytes <= 4 * 1024 * 1024,
            "invalid joint range cap"
        );
        ensure!(
            !policies[..i].iter().any(|q| q.source == p.source),
            "duplicate joint source id"
        );
    }
    ensure!(
        buffer_bound(f, limits)? <= limits.max_planner_bytes,
        "joint planner memory bound exceeded"
    );
    let n = f.nodes.len();
    let mut parents = vec![0usize; n];
    let mut spans = 0usize;
    for (i, node) in f.nodes.iter().enumerate() {
        ensure!(
            !f.nodes[..i].iter().any(|q| q.id == node.id),
            "duplicate joint node id"
        );
        let mut check_action = |a: &Action| -> Result<()> {
            spans = add(spans, a.reads.len())?;
            ensure!(spans <= MAX_SPANS, "joint span bound exceeded");
            for r in &a.reads {
                let p = policy(policies, r.source)?;
                ensure!(
                    r.len > 0
                        && r.len <= p.max_range_bytes
                        && r.offset.checked_add(r.len).is_some(),
                    "invalid indivisible joint read span"
                );
            }
            Ok(())
        };
        if let Some(a) = &node.summary {
            check_action(a)?;
        }
        match &node.fallback {
            Fallback::Raw(a) => check_action(a)?,
            Fallback::Children(children) => {
                ensure!(!children.is_empty(), "empty joint child list");
                for &c in children {
                    ensure!(c < n && c != i, "invalid joint child");
                    parents[c] += 1;
                    ensure!(parents[c] <= 1, "joint input is not a forest");
                }
            }
            _ => {}
        }
    }
    let mut root_seen = vec![false; n];
    for &r in &f.roots {
        ensure!(
            r < n && parents[r] == 0 && !root_seen[r],
            "invalid joint root"
        );
        root_seen[r] = true;
    }
    ensure!(
        (0..n).all(|i| parents[i] > 0 || root_seen[i]),
        "unlisted joint root"
    );
    let mut order = Vec::with_capacity(n);
    let mut stack = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    stack.extend(f.roots.iter().rev().copied());
    while let Some(i) = stack.pop() {
        ensure!(!seen[i], "cycle in joint forest");
        seen[i] = true;
        order.push(i);
        if let Fallback::Children(c) = &f.nodes[i].fallback {
            stack.extend(c.iter().rev().copied());
        }
    }
    ensure!(order.len() == n, "cycle or unreachable joint node");
    let mut ranks = vec![0usize; n];
    for (r, &i) in order.iter().enumerate() {
        ranks[i] = r;
    }
    order.reverse();
    let mut counts = vec![0usize; n];
    let mut fallback_counts = vec![0usize; n];
    let cap = limits.max_states + 1;
    for &i in &order {
        let fallback = match &f.nodes[i].fallback {
            Fallback::Raw(_) => 1,
            Fallback::Unavailable => 0,
            Fallback::Children(c) => c
                .iter()
                .fold(1usize, |a, &j| a.saturating_mul(counts[j]).min(cap)),
        };
        fallback_counts[i] = fallback;
        counts[i] =
            (fallback + usize::from(f.nodes[i].certified && f.nodes[i].summary.is_some())).min(cap);
    }
    ensure!(
        f.roots.iter().all(|&r| counts[r] > 0),
        "joint forest has no legal cover"
    );
    Ok(Validated {
        counts,
        fallback_counts,
        postorder: order,
        ranks,
        spans,
    })
}
fn score(model: CostModel, requests: usize, bytes: u64, cpu: u64) -> Result<u128> {
    let ns = (requests as u128)
        .checked_mul(model.request_latency_ns as u128)
        .and_then(|n| n.checked_add(cpu as u128));
    ns.and_then(|n| n.checked_mul(model.bandwidth_bytes_per_second as u128))
        .and_then(|n| n.checked_add((bytes as u128) * 1_000_000_000))
        .ok_or_else(|| anyhow!("joint cost overflow"))
}
fn evaluate(
    f: &Forest,
    choices: &[Choice],
    policies: &[SourcePolicy],
    model: CostModel,
    span_capacity: usize,
    request_first: bool,
    work: &mut Work<'_>,
) -> Result<Evaluation> {
    check_cancel(work.cancel)?;
    work.diagnostics.covers_evaluated += 1;
    let mut spans = Vec::with_capacity(span_capacity);
    let mut cpu = 0u64;
    for &c in choices {
        let a = action(f, c);
        cpu = cpu
            .checked_add(a.reduction_ns)
            .ok_or_else(|| anyhow!("joint CPU cost overflow"))?;
        spans.extend_from_slice(&a.reads);
    }
    spans.sort_unstable_by(|a, b| {
        (a.source, a.offset)
            .cmp(&(b.source, b.offset))
            .then_with(|| b.len.cmp(&a.len))
    });
    // A contained demand is served by its containing demand. Keep noncontained
    // overlapping spans; indivisible requests may require overlapping reads.
    let mut keep = 0usize;
    for i in 0..spans.len() {
        let s = spans[i];
        if keep > 0 && spans[keep - 1].source == s.source && spans[keep - 1].end() >= s.end() {
            continue;
        }
        spans[keep] = s;
        keep += 1;
    }
    spans.truncate(keep);
    let mut unique = 0u64;
    let mut last: Option<ReadSpan> = None;
    for &s in &spans {
        let extra = if let Some(p) = last {
            if p.source == s.source {
                s.end() - p.end().max(s.offset)
            } else {
                s.len
            }
        } else {
            s.len
        };
        unique = unique
            .checked_add(extra)
            .ok_or_else(|| anyhow!("joint unique byte overflow"))?;
        last = Some(s);
    }
    let mut ranges = Vec::with_capacity(spans.len());
    let mut total_bytes = 0u64;
    let mut begin = 0usize;
    while begin < spans.len() {
        let p = policy(policies, spans[begin].source)?;
        let mut stop = begin + 1;
        while stop < spans.len() && spans[stop].source == p.source {
            stop += 1;
        }
        let ss = &spans[begin..stop];
        let zero = RangeState {
            score: 0,
            bytes: 0,
            requests: 0,
            previous: 0,
        };
        let mut states = vec![zero; ss.len() + 1];
        for end in 1..=ss.len() {
            let mut best: Option<RangeState> = None;
            for start in (0..end).rev() {
                work.transition()?;
                if start + 1 < end {
                    let gap = ss[start + 1].offset.saturating_sub(ss[start].end());
                    if gap
                        > if p.strict_no_overread {
                            0
                        } else {
                            p.max_gap_bytes
                        }
                    {
                        break;
                    }
                }
                let len = ss[end - 1].end() - ss[start].offset;
                if len > p.max_range_bytes {
                    break;
                }
                let prev = states[start];
                let bytes = prev
                    .bytes
                    .checked_add(len)
                    .ok_or_else(|| anyhow!("joint range byte overflow"))?;
                let state = RangeState {
                    score: score(model, prev.requests + 1, bytes, 0)?,
                    bytes,
                    requests: prev.requests + 1,
                    previous: start,
                };
                let better = best.is_none_or(|b| {
                    if request_first {
                        (state.requests, state.bytes) < (b.requests, b.bytes)
                    } else {
                        (state.score, state.requests, state.bytes) < (b.score, b.requests, b.bytes)
                    }
                });
                if better {
                    best = Some(state);
                }
            }
            states[end] = best.ok_or_else(|| anyhow!("no bounded joint range partition"))?;
        }
        total_bytes = total_bytes
            .checked_add(states[ss.len()].bytes)
            .ok_or_else(|| anyhow!("joint total byte overflow"))?;
        let first_range = ranges.len();
        let mut end = ss.len();
        while end > 0 {
            let start = states[end].previous;
            ranges.push(ReadSpan {
                source: p.source,
                offset: ss[start].offset,
                len: ss[end - 1].end() - ss[start].offset,
            });
            end = start;
        }
        ranges[first_range..].reverse();
        begin = stop;
    }
    let numerator = score(model, ranges.len(), total_bytes, cpu)?;
    let ordinary = Evaluation {
        cost: Cost {
            requests: ranges.len(),
            fetched_bytes: total_bytes,
            unique_requested_bytes: unique,
            reduction_ns: cpu,
            score_numerator: numerator,
            estimated_ns: numerator as f64 / model.bandwidth_bytes_per_second as f64,
        },
        ranges,
    };
    if feasible(ordinary.cost, work.limits) || unique > work.limits.max_total_read_bytes {
        return Ok(ordinary);
    }
    // A single cheapest-prefix state is insufficient when a query-wide byte or
    // request cap binds. Solve the bounded Pareto problem rather than silently
    // calling a feasible but suboptimal partition exact.
    ensure!(
        spans.len() <= 64,
        "joint constrained-range state bound exceeded"
    );
    Ok(
        constrained_ranges(&spans, policies, model, cpu, unique, request_first, work)?
            .unwrap_or(ordinary),
    )
}
fn constrained_ranges(
    spans: &[ReadSpan],
    policies: &[SourcePolicy],
    model: CostModel,
    cpu: u64,
    unique: u64,
    request_first: bool,
    work: &mut Work<'_>,
) -> Result<Option<Evaluation>> {
    let n = spans.len();
    let max_requests = n.min(work.limits.max_requests);
    let stride = max_requests + 1;
    let unreachable = ConstrainedState {
        bytes: u64::MAX,
        previous: 0,
    };
    let mut states = vec![unreachable; (n + 1) * stride];
    states[0] = ConstrainedState {
        bytes: 0,
        previous: 0,
    };
    for end in 1..=n {
        let p = policy(policies, spans[end - 1].source)?;
        for start in (0..end).rev() {
            if spans[start].source != p.source {
                break;
            }
            if start + 1 < end {
                let gap = spans[start + 1].offset.saturating_sub(spans[start].end());
                if gap
                    > if p.strict_no_overread {
                        0
                    } else {
                        p.max_gap_bytes
                    }
                {
                    break;
                }
            }
            let len = spans[end - 1].end() - spans[start].offset;
            if len > p.max_range_bytes {
                break;
            }
            for requests in 1..=max_requests.min(end) {
                work.transition()?;
                let prev = states[start * stride + requests - 1];
                if let Some(bytes) = prev.bytes.checked_add(len) {
                    if bytes <= work.limits.max_total_read_bytes
                        && bytes < states[end * stride + requests].bytes
                    {
                        states[end * stride + requests] = ConstrainedState {
                            bytes,
                            previous: start,
                        };
                    }
                }
            }
        }
    }
    let mut best: Option<(usize, u64, u128)> = None;
    for requests in 1..=max_requests {
        let bytes = states[n * stride + requests].bytes;
        if bytes == u64::MAX {
            continue;
        }
        let cost = score(model, requests, bytes, cpu)?;
        if best.is_none_or(|(r, b, c)| {
            if request_first {
                (requests, bytes) < (r, b)
            } else {
                (cost, requests, bytes) < (c, r, b)
            }
        }) {
            best = Some((requests, bytes, cost));
        }
    }
    let Some((requests, bytes, numerator)) = best else {
        return Ok(None);
    };
    let mut ranges = Vec::with_capacity(requests);
    let mut end = n;
    let mut r = requests;
    while end > 0 {
        let start = states[end * stride + r].previous;
        ranges.push(ReadSpan {
            source: spans[start].source,
            offset: spans[start].offset,
            len: spans[end - 1].end() - spans[start].offset,
        });
        end = start;
        r -= 1;
    }
    ranges.reverse();
    Ok(Some(Evaluation {
        cost: Cost {
            requests,
            fetched_bytes: bytes,
            unique_requested_bytes: unique,
            reduction_ns: cpu,
            score_numerator: numerator,
            estimated_ns: numerator as f64 / model.bandwidth_bytes_per_second as f64,
        },
        ranges,
    }))
}
fn build_cover(
    f: &Forest,
    v: &Validated,
    roots: &[usize],
    mut choose: impl FnMut(usize) -> bool,
) -> Vec<Choice> {
    let mut stack = Vec::with_capacity(f.nodes.len());
    stack.extend(roots.iter().rev().copied());
    let mut out = Vec::with_capacity(f.nodes.len());
    while let Some(i) = stack.pop() {
        let node = &f.nodes[i];
        let eligible = node.certified && node.summary.is_some();
        if eligible && (v.fallback_counts[i] == 0 || choose(i)) {
            out.push(Choice {
                node: i,
                kind: ChoiceKind::Summary,
            });
        } else {
            match &node.fallback {
                Fallback::Children(c) => stack.extend(c.iter().rev().copied()),
                Fallback::Raw(_) => out.push(Choice {
                    node: i,
                    kind: ChoiceKind::Raw,
                }),
                Fallback::Unavailable => unreachable!(),
            }
        }
    }
    out
}
fn feasible(c: Cost, l: Limits) -> bool {
    c.requests <= l.max_requests && c.fetched_bytes <= l.max_total_read_bytes
}
fn consider(
    best: &mut Option<Plan>,
    choices: &[Choice],
    eval: Evaluation,
    mode: Mode,
    limits: Limits,
) {
    if feasible(eval.cost, limits) && best.as_ref().is_none_or(|b| eval.cost.key() < b.cost.key()) {
        *best = Some(Plan {
            choices: choices.to_vec(),
            ranges: eval.ranges,
            cost: eval.cost,
            exact: false,
            mode,
            diagnostics: Diagnostics::default(),
        });
    }
}
/// The caller certifies geometry/statistic eligibility and supplies only legal
/// raw alternatives. Gap restrictions do not make an illegal raw action legal.
pub fn plan(
    f: &Forest,
    policies: &[SourcePolicy],
    model: CostModel,
    limits: Limits,
    mode: Mode,
    cancel: &AtomicBool,
) -> Result<Plan> {
    check_cancel(cancel)?;
    let v = validate(f, policies, model, limits)?;
    let count = f.roots.iter().fold(1usize, |n, &r| {
        n.saturating_mul(v.counts[r]).min(limits.max_states + 1)
    });
    let mut work = Work {
        diagnostics: Diagnostics {
            cover_count_capped: count,
            planner_buffer_bound: buffer_bound(f, limits)?,
            ..Diagnostics::default()
        },
        limits,
        cancel,
    };
    let mut best = None;
    let mut exact = false;
    match mode {
        Mode::FixedRequestFirst => {
            let cover = build_cover(f, &v, &f.roots, |_| true);
            let eval = evaluate(f, &cover, policies, model, v.spans, true, &mut work)?;
            consider(&mut best, &cover, eval, mode, limits);
        }
        Mode::Greedy => {
            let mut pref = vec![false; f.nodes.len()];
            for &i in &v.postorder {
                let n = &f.nodes[i];
                if !(n.certified && n.summary.is_some()) {
                    continue;
                }
                if v.fallback_counts[i] == 0 {
                    pref[i] = true;
                    continue;
                }
                let summary = [Choice {
                    node: i,
                    kind: ChoiceKind::Summary,
                }];
                let a = evaluate(f, &summary, policies, model, v.spans, false, &mut work)?;
                let child = build_cover(f, &v, &[i], |j| j != i && pref[j]);
                let b = evaluate(f, &child, policies, model, v.spans, false, &mut work)?;
                pref[i] = a.cost.key() <= b.cost.key();
            }
            let cover = build_cover(f, &v, &f.roots, |i| pref[i]);
            let eval = evaluate(f, &cover, policies, model, v.spans, false, &mut work)?;
            consider(&mut best, &cover, eval, mode, limits);
        }
        Mode::Joint if count <= limits.max_states => {
            // Variable-length branch decisions enumerate each legal complete
            // cover exactly once; descendants of a selected summary add no bits.
            let mut bits = Vec::with_capacity(f.nodes.len());
            loop {
                let mut used = 0usize;
                let cover = build_cover(f, &v, &f.roots, |_| {
                    if used == bits.len() {
                        bits.push(false);
                    }
                    let descend = bits[used];
                    used += 1;
                    !descend
                });
                bits.truncate(used);
                let eval = evaluate(f, &cover, policies, model, v.spans, false, &mut work)?;
                consider(&mut best, &cover, eval, mode, limits);
                if let Some(i) = bits.iter().rposition(|&bit| !bit) {
                    bits[i] = true;
                    bits.truncate(i + 1);
                } else {
                    break;
                }
            }
            debug_assert_eq!(work.diagnostics.covers_evaluated, count);
            exact = true;
        }
        Mode::Joint => {
            let cover = build_cover(f, &v, &f.roots, |_| true);
            let eval = evaluate(f, &cover, policies, model, v.spans, false, &mut work)?;
            let cost = eval.cost;
            consider(&mut best, &cover, eval, mode, limits);
            let mut beam = Vec::with_capacity(limits.beam_width);
            beam.push(Candidate {
                choices: cover,
                last_rank: None,
                cost,
            });
            while !beam.is_empty() && work.diagnostics.covers_evaluated < limits.max_states {
                work.diagnostics.beam_rounds += 1;
                let mut next: Vec<Candidate> = Vec::with_capacity(limits.beam_width);
                'states: for state in &beam {
                    for (pos, &c) in state.choices.iter().enumerate() {
                        let rank = v.ranks[c.node];
                        if c.kind != ChoiceKind::Summary
                            || v.fallback_counts[c.node] == 0
                            || state.last_rank.is_some_and(|last| rank <= last)
                        {
                            continue;
                        }
                        if work.diagnostics.covers_evaluated >= limits.max_states {
                            break 'states;
                        }
                        let replacement = build_cover(f, &v, &[c.node], |i| i != c.node);
                        let mut cover = Vec::with_capacity(f.nodes.len());
                        cover.extend_from_slice(&state.choices[..pos]);
                        cover.extend(replacement);
                        cover.extend_from_slice(&state.choices[pos + 1..]);
                        let eval = evaluate(f, &cover, policies, model, v.spans, false, &mut work)?;
                        let cost = eval.cost;
                        consider(&mut best, &cover, eval, mode, limits);
                        let candidate = Candidate {
                            choices: cover,
                            last_rank: Some(rank),
                            cost,
                        };
                        if next.len() < limits.beam_width {
                            next.push(candidate);
                        } else if cost.key() < next.last().unwrap().cost.key() {
                            next.pop();
                            next.push(candidate);
                        } else {
                            continue;
                        }
                        next.sort_unstable_by(|a, b| {
                            a.cost
                                .key()
                                .cmp(&b.cost.key())
                                .then_with(|| a.choices.cmp(&b.choices))
                        });
                    }
                }
                beam = next;
            }
            work.diagnostics.state_budget_exhausted =
                work.diagnostics.covers_evaluated >= limits.max_states;
        }
    }
    check_cancel(cancel)?;
    let mut result = best.ok_or_else(|| anyhow!("no joint cover fits I/O budget"))?;
    result.exact = exact;
    result.diagnostics = work.diagnostics;
    Ok(result)
}

// JSON has no portable 128-bit number representation; preserve the exact score.
mod decimal_u128 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
