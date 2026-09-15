# Performance evidence

The current entry point is the [final native Skarve + SKV report](../benchmarks/native-skv/REPORT.md). It identifies the exact d15 runtime, full latency, numerical policies, limits, individual losses and preparation costs. The newer launch API/documentation does not acquire those timings by renaming the version.

# Performance and benchmark evidence

The current [Skarve Benchmark Report](../benchmarks/beta2/report/index.html)
compares installed native and optional exactextract execution with natural
upstream controls. Its [raw evidence](../benchmarks/beta2/report/data/programme.json.gz)
retains the aggregate failure: 156 current/natural-control tasks passed their
numerical gates, while 12 historical isolated-adapter controls failed with
72,000 field mismatches and have no valid speed ratios. The current embedded
backend passed its separate matching-upstream checks.

The unchanged [beta1 report](../benchmarks/report/index.html) and its
[results](../benchmarks/results/) remain separate historical source/query,
shared-batch and typed-buffer evidence. [Reproduction tools](../benchmarks/README.md)
use generated fixtures requiring no private dataset or application. Consult each
result's engine hash, runtime versions, policy, workload and timing boundary
before comparing numbers.

Keep native strict, delegated exactextract, direct upstream controls and caller
interface costs distinct. A matching-policy reference checks bridge fidelity;
a native-versus-upstream numerical difference is not automatically a bridge
defect. Natural source adapters may interpret scaled Float32 values differently.

Retained decoded caches and summary indexes can win on suitable repeated jobs.
Cold sources, one-off requests and overflowing caches can lose. Whole lifecycle
timings include registration, preparation, useful result transfer and cleanup;
warm query timings do not stand in for those costs. The report preserves losses
and uncertainty without requiring universal superiority.

Run the harness's smoke mode first, then its documented frozen programme. Use
one timing lane and explicit memory/cache/thread budgets. Do not treat timings
from a different seed, source representation, output schema or old package as
measurements of the current candidate. Automatic backend eligibility is limited
to accepted [numerical and execution contracts](backends.md); the candidate's
measured disposition is stated with its benchmark results. Optional exactextract
is supported for explicit eligible upstream-policy requests. Native strict stays
the default; no performance-driven cross-policy auto promotion follows from this
finite evaluation. A first or empty decoded cache does not imply cold storage:
the operating-system page cache was not flushed.
