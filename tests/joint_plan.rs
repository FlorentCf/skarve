use raster_engine::joint_plan::*;
use std::sync::atomic::AtomicBool;
fn raw(source: u32, offset: u64, len: u64, cpu: u64) -> Action {
    Action {
        reads: vec![ReadSpan {
            source,
            offset,
            len,
        }],
        reduction_ns: cpu,
    }
}
fn policy(cap: u64, strict: bool) -> Vec<SourcePolicy> {
    vec![
        SourcePolicy {
            source: 0,
            max_range_bytes: cap,
            max_gap_bytes: 10000,
            strict_no_overread: strict,
        },
        SourcePolicy {
            source: 1,
            max_range_bytes: cap,
            max_gap_bytes: 10000,
            strict_no_overread: strict,
        },
    ]
}
fn model(latency: u64) -> CostModel {
    CostModel {
        request_latency_ns: latency,
        bandwidth_bytes_per_second: 1_000_000_000,
    }
}
fn coupled(roots: usize) -> Forest {
    let mut f = Forest {
        nodes: vec![],
        roots: vec![],
    };
    for r in 0..roots {
        let i = f.nodes.len();
        f.roots.push(i);
        f.nodes.push(Node {
            id: i as u64,
            certified: true,
            summary: Some(raw(0, 10000 + r as u64 * 100, 1, 0)),
            fallback: Fallback::Children(vec![i + 1, i + 2]),
        });
        f.nodes.push(Node {
            id: (i + 1) as u64,
            certified: false,
            summary: None,
            fallback: Fallback::Raw(raw(0, (2 * r) as u64, 1, 0)),
        });
        f.nodes.push(Node {
            id: (i + 2) as u64,
            certified: false,
            summary: None,
            fallback: Fallback::Raw(raw(0, (2 * r + 1) as u64, 1, 0)),
        });
    }
    f
}
#[test]
fn coupled_cover_beats_independent_greedy_and_fixed_cover() {
    let f = coupled(2);
    let p = policy(4, true);
    let c = AtomicBool::new(false);
    let joint = plan(&f, &p, model(100), Limits::default(), Mode::Joint, &c).unwrap();
    let greedy = plan(&f, &p, model(100), Limits::default(), Mode::Greedy, &c).unwrap();
    let fixed = plan(
        &f,
        &p,
        model(100),
        Limits::default(),
        Mode::FixedRequestFirst,
        &c,
    )
    .unwrap();
    assert!(joint.exact);
    assert_eq!(joint.diagnostics.covers_evaluated, 4);
    assert_eq!((joint.cost.requests, joint.cost.fetched_bytes), (1, 4));
    assert_eq!(joint.cost.estimated_ns, 104.0);
    assert_eq!(greedy.cost.estimated_ns, 202.0);
    assert_eq!(fixed.cost.estimated_ns, 202.0);
    assert!(joint.choices.iter().all(|c| c.kind == ChoiceKind::Raw));
}
#[test]
fn bandwidth_gap_caps_and_binding_global_budgets() {
    let f = Forest {
        roots: vec![0],
        nodes: vec![Node {
            id: 0,
            certified: false,
            summary: None,
            fallback: Fallback::Raw(Action {
                reads: vec![
                    ReadSpan {
                        source: 0,
                        offset: 0,
                        len: 1,
                    },
                    ReadSpan {
                        source: 0,
                        offset: 1000,
                        len: 1,
                    },
                ],
                reduction_ns: 0,
            }),
        }],
    };
    let c = AtomicBool::new(false);
    let p = policy(2000, false);
    let joint = plan(&f, &p, model(10), Limits::default(), Mode::Joint, &c).unwrap();
    assert_eq!((joint.cost.requests, joint.cost.fetched_bytes), (2, 2));
    let fixed = plan(
        &f,
        &p,
        model(10),
        Limits::default(),
        Mode::FixedRequestFirst,
        &c,
    )
    .unwrap();
    assert_eq!((fixed.cost.requests, fixed.cost.fetched_bytes), (1, 1001));
    let mut l = Limits::default();
    l.max_total_read_bytes = 2;
    let constrained = plan(&f, &p, model(2000), l, Mode::Joint, &c).unwrap();
    assert!(constrained.exact);
    assert_eq!(
        (constrained.cost.requests, constrained.cost.fetched_bytes),
        (2, 2)
    );
    l.max_requests = 1;
    assert!(plan(&f, &p, model(2000), l, Mode::Joint, &c).is_err());
    let strict = plan(
        &f,
        &policy(2000, true),
        model(2000),
        Limits::default(),
        Mode::Joint,
        &c,
    )
    .unwrap();
    assert_eq!(strict.cost.requests, 2);
}
#[test]
fn identical_contained_and_indivisible_overlapping_demands() {
    let f = Forest {
        roots: vec![0],
        nodes: vec![Node {
            id: 0,
            certified: false,
            summary: None,
            fallback: Fallback::Raw(Action {
                reads: vec![
                    ReadSpan {
                        source: 0,
                        offset: 0,
                        len: 4,
                    },
                    ReadSpan {
                        source: 0,
                        offset: 0,
                        len: 4,
                    },
                    ReadSpan {
                        source: 0,
                        offset: 1,
                        len: 2,
                    },
                    ReadSpan {
                        source: 0,
                        offset: 2,
                        len: 4,
                    },
                ],
                reduction_ns: 3,
            }),
        }],
    };
    let p = plan(
        &f,
        &policy(4, true),
        model(100),
        Limits::default(),
        Mode::Joint,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        (
            p.cost.requests,
            p.cost.fetched_bytes,
            p.cost.unique_requested_bytes
        ),
        (2, 8, 6)
    );
    assert_eq!(p.cost.reduction_ns, 3);
}
#[test]
fn bounded_beam_reports_approximation_and_a_real_budget_loss() {
    let f = coupled(20);
    let mut limits = Limits::default();
    limits.max_states = 32;
    limits.beam_width = 2;
    let p = plan(
        &f,
        &policy(40, true),
        model(100),
        limits,
        Mode::Joint,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!p.exact);
    assert!(p.diagnostics.state_budget_exhausted);
    assert_eq!(p.diagnostics.covers_evaluated, 32);
    assert!(p.cost.estimated_ns > 140.0); // All forty terminal spans fit one 40-byte range.
    assert!(p.diagnostics.planner_buffer_bound <= limits.max_planner_bytes);
}
// Independent oracle: recursively form covers, then enumerate arbitrary physical
// range covers over a bitmask of indivisible demands. It does not use the native
// prefix DP or its ordered-partition restriction.
fn covers(f: &Forest, i: usize) -> Vec<Vec<Choice>> {
    let mut out = vec![];
    let n = &f.nodes[i];
    if n.certified && n.summary.is_some() {
        out.push(vec![Choice {
            node: i,
            kind: ChoiceKind::Summary,
        }]);
    }
    match &n.fallback {
        Fallback::Raw(_) => out.push(vec![Choice {
            node: i,
            kind: ChoiceKind::Raw,
        }]),
        Fallback::Unavailable => {}
        Fallback::Children(children) => {
            let mut combos = vec![vec![]];
            for &child in children {
                let options = covers(f, child);
                let mut next = vec![];
                for a in &combos {
                    for b in &options {
                        let mut x = a.clone();
                        x.extend_from_slice(b);
                        next.push(x);
                    }
                }
                combos = next;
            }
            out.extend(combos);
        }
    }
    out
}
fn all_covers(f: &Forest) -> Vec<Vec<Choice>> {
    let mut all = vec![vec![]];
    for &r in &f.roots {
        let options = covers(f, r);
        let mut next = vec![];
        for a in &all {
            for b in &options {
                let mut x = a.clone();
                x.extend_from_slice(b);
                next.push(x);
            }
        }
        all = next;
    }
    all
}
fn oracle(
    f: &Forest,
    policies: &[SourcePolicy],
    m: CostModel,
    l: Limits,
) -> Option<(u128, usize, u64)> {
    let mut optimum = None;
    for cover in all_covers(f) {
        let mut demands = vec![];
        let mut cpu = 0u64;
        for c in cover {
            let a = match c.kind {
                ChoiceKind::Summary => f.nodes[c.node].summary.as_ref().unwrap(),
                ChoiceKind::Raw => match &f.nodes[c.node].fallback {
                    Fallback::Raw(a) => a,
                    _ => unreachable!(),
                },
            };
            demands.extend_from_slice(&a.reads);
            cpu += a.reduction_ns;
        }
        let mut reads: Vec<(u64, u64)> = vec![];
        for a in &demands {
            for b in &demands {
                if a.source != b.source || b.offset + b.len <= a.offset {
                    continue;
                }
                let lo = a.offset;
                let hi = b.offset + b.len;
                let p = policies.iter().find(|p| p.source == a.source).unwrap();
                if hi - lo > p.max_range_bytes {
                    continue;
                }
                let mut pieces: Vec<_> = demands
                    .iter()
                    .filter(|d| d.source == a.source && d.offset < hi && d.offset + d.len > lo)
                    .map(|d| (d.offset.max(lo), (d.offset + d.len).min(hi)))
                    .collect();
                pieces.sort_unstable();
                let mut reach = lo;
                let mut allowed = true;
                for (x, y) in pieces {
                    if x > reach
                        && x - reach
                            > if p.strict_no_overread {
                                0
                            } else {
                                p.max_gap_bytes
                            }
                    {
                        allowed = false;
                        break;
                    }
                    reach = reach.max(y);
                }
                if !allowed || reach < hi {
                    continue;
                }
                let mut mask = 0u64;
                for (i, d) in demands.iter().enumerate() {
                    if d.source == a.source && d.offset >= lo && d.offset + d.len <= hi {
                        mask |= 1 << i;
                    }
                }
                if mask != 0 && !reads.contains(&(mask, hi - lo)) {
                    reads.push((mask, hi - lo));
                }
            }
        }
        fn visit(
            mask: u64,
            requests: usize,
            bytes: u64,
            target: u64,
            reads: &[(u64, u64)],
            cpu: u64,
            m: CostModel,
            l: Limits,
            best: &mut Option<(u128, usize, u64)>,
        ) {
            if requests > l.max_requests || bytes > l.max_total_read_bytes {
                return;
            }
            let numerator = ((requests as u128 * m.request_latency_ns as u128) + cpu as u128)
                * m.bandwidth_bytes_per_second as u128
                + bytes as u128 * 1_000_000_000;
            let key = (numerator, requests, bytes);
            if best.is_some_and(|b| key >= b) {
                return;
            }
            if mask == target {
                *best = Some(key);
                return;
            }
            let first = (!mask & target).trailing_zeros();
            for &(covers, n) in reads {
                if covers & (1 << first) != 0 {
                    visit(
                        mask | covers,
                        requests + 1,
                        bytes + n,
                        target,
                        reads,
                        cpu,
                        m,
                        l,
                        best,
                    );
                }
            }
        }
        visit(
            0,
            0,
            0,
            (1 << demands.len()) - 1,
            &reads,
            cpu,
            m,
            l,
            &mut optimum,
        );
    }
    optimum
}
#[test]
fn joint_matches_independent_arbitrary_range_cover_oracle() {
    let mut state = 8127u64;
    let mut rand = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 32
    };
    let cancel = AtomicBool::new(false);
    let mut cases = 0;
    for trial in 0..128 {
        let mut f = coupled(1 + (trial % 3));
        for node in &mut f.nodes {
            if let Some(a) = &mut node.summary {
                a.reads = vec![ReadSpan {
                    source: (rand() % 2) as u32,
                    offset: rand() % 16,
                    len: 1 + rand() % 3,
                }];
                a.reduction_ns = rand() % 4;
            }
            if let Fallback::Raw(a) = &mut node.fallback {
                a.reads = vec![ReadSpan {
                    source: (rand() % 2) as u32,
                    offset: rand() % 16,
                    len: 1 + rand() % 3,
                }];
                a.reduction_ns = rand() % 4;
            }
        }
        for strict in [false, true] {
            for latency in [0, 5, 50] {
                for bounded in [false, true] {
                    let p = policy(if trial % 2 == 0 { 4 } else { 8 }, strict);
                    let mut l = Limits::default();
                    if bounded {
                        l.max_requests = 2;
                        l.max_total_read_bytes = 8;
                    }
                    let want = oracle(&f, &p, model(latency), l);
                    let got = plan(&f, &p, model(latency), l, Mode::Joint, &cancel);
                    match want {
                        Some((score, requests, bytes)) => {
                            let got=got.unwrap_or_else(|e|panic!("trial {trial} strict {strict} latency {latency} bounded {bounded}: {e}"));
                            assert!(got.exact);
                            assert_eq!(
                                (
                                    got.cost.score_numerator,
                                    got.cost.requests,
                                    got.cost.fetched_bytes
                                ),
                                (score, requests, bytes),
                                "trial {trial}, strict {strict}, latency {latency}, bounded {bounded}"
                            );
                        }
                        None => assert!(got.is_err(), "oracle found no feasible cover"),
                    };
                    cases += 1;
                }
            }
        }
    }
    println!("independent arbitrary-range oracle cases: {cases}");
    assert_eq!(cases, 1536);
}
#[test]
fn rejects_invalid_forests_bounds_and_cancellation() {
    let f = coupled(2);
    let p = policy(4, true);
    let c = AtomicBool::new(false);
    assert!(
        plan(
            &f,
            &p,
            model(1),
            Limits::default(),
            Mode::Joint,
            &AtomicBool::new(true)
        )
        .is_err()
    );
    let mut bad = f.clone();
    bad.nodes[1].fallback = Fallback::Children(vec![0]);
    assert!(plan(&bad, &p, model(1), Limits::default(), Mode::Joint, &c).is_err());
    let mut bad = f.clone();
    bad.nodes[0].certified = false;
    bad.nodes[0].fallback = Fallback::Unavailable;
    bad.nodes.truncate(1);
    bad.roots = vec![0];
    assert!(plan(&bad, &p, model(1), Limits::default(), Mode::Joint, &c).is_err());
    let mut l = Limits::default();
    l.max_planner_bytes = 1;
    assert!(plan(&f, &p, model(1), l, Mode::Joint, &c).is_err());
    l = Limits::default();
    l.max_range_transitions = 1;
    assert!(plan(&f, &p, model(1), l, Mode::Joint, &c).is_err());
    let mut bad = f.clone();
    bad.nodes[0].summary.as_mut().unwrap().reads[0].offset = u64::MAX;
    assert!(plan(&bad, &p, model(1), Limits::default(), Mode::Joint, &c).is_err());
    assert!(
        plan(
            &f,
            &p,
            CostModel {
                request_latency_ns: 1,
                bandwidth_bytes_per_second: 0
            },
            Limits::default(),
            Mode::Joint,
            &c
        )
        .is_err()
    );
}

#[test]
fn binding_large_range_frontier_fails_explicitly_and_json_scores_are_lossless() {
    let forest = Forest {
        nodes: (0..65)
            .map(|i| Node {
                id: i,
                certified: false,
                summary: None,
                fallback: Fallback::Raw(Action {
                    reads: vec![ReadSpan {
                        source: 0,
                        offset: i * 10,
                        len: 1,
                    }],
                    reduction_ns: 0,
                }),
            })
            .collect(),
        roots: (0..65).collect(),
    };
    let policies = [SourcePolicy {
        source: 0,
        max_range_bytes: 1024,
        max_gap_bytes: 1024,
        strict_no_overread: false,
    }];
    let mut limits = Limits::default();
    limits.max_total_read_bytes = 65;
    let err = plan(
        &forest,
        &policies,
        model(1_000_000),
        limits,
        Mode::Joint,
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("constrained-range state bound exceeded")
    );
    let result = plan(
        &coupled(2),
        &policy(4, true),
        CostModel {
            request_latency_ns: 1_000_000_000,
            bandwidth_bytes_per_second: 100_000_000_000,
        },
        Limits::default(),
        Mode::Joint,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(result.cost.score_numerator > u64::MAX as u128);
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        encoded["cost"]["score_numerator"].as_str().unwrap(),
        result.cost.score_numerator.to_string()
    );
    let decoded: Plan = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.cost.score_numerator, result.cost.score_numerator);
}
