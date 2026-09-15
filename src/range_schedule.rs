//! Optimal ordered partition of known summary records under a range-size cap.
//! Objective: fewest requests, then fewest fetched records. This does not claim
//! to minimize latency for an unknown link, or to choose optimal tree nodes.
use crate::model::check_cancel;
use anyhow::{Result, ensure};
use std::sync::atomic::AtomicBool;

pub(crate) const MAX_RECORDS: usize = 4096;
#[derive(Clone, Copy)]
struct State {
    requests: usize,
    records: usize,
    previous: usize,
}
pub(crate) fn buffer_bound(count: usize) -> usize {
    // Caller task triples, sorted record IDs, DP state, and resulting ranges.
    count
        * (std::mem::size_of::<(usize, usize, usize)>()
            + std::mem::size_of::<usize>()
            + std::mem::size_of::<(usize, usize)>())
        + (count + 1) * std::mem::size_of::<State>()
}
pub(crate) fn plan(
    records: &[usize],
    cap: usize,
    cancel: &AtomicBool,
) -> Result<Vec<(usize, usize)>> {
    ensure!(
        records.len() <= MAX_RECORDS && cap > 0,
        "summary schedule bound exceeded"
    );
    ensure!(
        records.windows(2).all(|p| p[0] < p[1]),
        "summary records must be unique and ordered"
    );
    let mut states = vec![
        State {
            requests: 0,
            records: 0,
            previous: 0
        };
        records.len() + 1
    ];
    for end in 1..=records.len() {
        if end % 64 == 0 {
            check_cancel(cancel)?;
        }
        let mut best = State {
            requests: usize::MAX,
            records: usize::MAX,
            previous: 0,
        };
        for start in (0..end).rev() {
            let span = records[end - 1]
                .checked_sub(records[start])
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("summary range overflow"))?;
            if span > cap {
                break;
            }
            let candidate = State {
                requests: states[start].requests + 1,
                records: states[start]
                    .records
                    .checked_add(span)
                    .ok_or_else(|| anyhow::anyhow!("summary cost overflow"))?,
                previous: start,
            };
            if (candidate.requests, candidate.records) < (best.requests, best.records) {
                best = candidate;
            }
        }
        states[end] = best;
    }
    let mut ranges = Vec::with_capacity(records.len());
    let mut end = records.len();
    while end > 0 {
        let start = states[end].previous;
        ranges.push((records[start], records[end - 1] - records[start] + 1));
        end = start;
    }
    // Reverse file order allows allocation-free pop() by the caller.
    check_cancel(cancel)?;
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedule_matches_exhaustive_partitions_and_beats_greedy_byte_waste() {
        let cancel = AtomicBool::new(false);
        assert_eq!(
            plan(&[0, 1, 4, 5], 5, &cancel).unwrap(),
            vec![(4, 2), (0, 2)]
        );
        for mask in 1usize..1 << 10 {
            let records: Vec<_> = (0..10).filter(|i| mask & (1 << i) != 0).collect();
            for cap in [1, 2, 4, 8] {
                let mut optimum = (usize::MAX, usize::MAX);
                // Independently enumerate every possible ordered partition.
                for cuts in 0usize..1 << (records.len() - 1) {
                    let (mut first, mut requests, mut bytes, mut valid) = (0, 0, 0, true);
                    for last in 0..records.len() {
                        if last == records.len() - 1 || cuts & (1 << last) != 0 {
                            let span = records[last] - records[first] + 1;
                            if span > cap {
                                valid = false;
                                break;
                            }
                            requests += 1;
                            bytes += span;
                            first = last + 1;
                        }
                    }
                    if valid {
                        optimum = optimum.min((requests, bytes));
                    }
                }
                let ranges = plan(&records, cap, &cancel).unwrap();
                assert_eq!((ranges.len(), ranges.iter().map(|r| r.1).sum()), optimum);
                assert!(records.iter().all(|p| {
                    ranges
                        .iter()
                        .filter(|&&(start, n)| *p >= start && *p < start + n)
                        .count()
                        == 1
                }));
            }
        }
    }
    #[test]
    fn rejects_invalid_or_cancelled_schedules() {
        assert!(plan(&[1, 1], 4, &AtomicBool::new(false)).is_err());
        assert!(plan(&[2, 1], 4, &AtomicBool::new(false)).is_err());
        assert!(plan(&[usize::MAX], 0, &AtomicBool::new(false)).is_err());
        assert!(plan(&(0..4097).collect::<Vec<_>>(), 16, &AtomicBool::new(false)).is_err());
        assert!(plan(&(0..128).collect::<Vec<_>>(), 16, &AtomicBool::new(true)).is_err());
    }
}
