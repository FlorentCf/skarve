# Failure and cancellation costs

Twelve installed-package observations check rejection, active cancellation and
recovery: three each for Python/Node × native/exactextract. These are lifecycle
observations on the same Ubuntu 24.04 WSL2 host as the beta.2 qualification, not
p95 estimates or an independent hardware comparison. The immutable checkpoint02
wheel and npm archive both load library
`4c20fbc0e5486c0f4d3a0a457664211074ff4c65c21fb412fa268943e7a57818`.

A request with an incompatible numerical policy fails without a new source GET.
For cancellation, a local range server holds one **observed active** source read.
The consumer initiates cancellation, waits at least 25 ms, confirms that the call
has not returned, and releases the read. The awaiter then reports cancellation.
This deliberately includes controlled I/O wait and local control-request cost.
It does not measure cancellation solely inside a C++ geometry phase or establish
a hard deadline. No partial answer is accepted as a successful result.

All twelve cases leave the interrupted remote reader quarantined with
`remote source transport failed: cancelled`. Closing it and reopening the original
specification within the same session produces the expected numerical answer.
This is a recovery requirement for the observed interrupted remote-read case,
not evidence that every cancellation invalidates its source.

Medians below summarize three raw observations per cell, in milliseconds:

| Runtime/backend | Policy rejection | Cancel initiation | Cancel → drain | Read release → drain | Close/reopen/query/close recovery |
|---|---:|---:|---:|---:|---:|
| Python native | 0.088 | 0.006 | 27.924 | 2.208 | 19.612 |
| Python exactextract | 0.063 | 0.004 | 27.324 | 1.754 | 23.110 |
| Node native | 0.426 | 0.075 | 27.085 | 1.970 | 18.980 |
| Node exactextract | 0.242 | 0.063 | 27.515 | 1.821 | 23.260 |

The controlled hold accounts for roughly 25–26 ms of cancel-to-drain. Python task
cancellation and Node AbortController initiation have different runtime paths;
their tiny samples are not a ranking. Child processes use a 2 GiB address-space
limit and 120-second harness timeout. Those harness limits do not turn the
embedded engine into a hard-RSS or hard-cancellation interface.

[Raw receipt](failure-cancellation.json) contains all samples, actual hold time,
rejection GET counts, read enter/release events, error outcomes, recovery checks,
immutable input hashes and script hashes. The original baseline benchmark files
and scores are unchanged. Three earlier development attempts remain in private
evidence: an initial expectation of same-reader reuse exposed the quarantine;
Node's test-only fetch implementation exceeded its Wasm address reservation
under the harness limit and was replaced with bounded core HTTP; and a passing
probe reopened before closing the old reader. The final probe explicitly closes
the failed reader before reopening it. No engine change was needed.

Reproduce from the source archive with qualified beta.2 artifacts and the local
fixture wheelhouse used by the installed suite:

```sh
python3 benchmarks/beta2/failure_cancellation.py run \
  --artifacts /path/to/beta2-exactextract-artifacts \
  --wheelhouse /path/to/test-wheelhouse \
  --output-dir /tmp/skarve-cancellation-fresh \
  --repeats 3
```

The destination must be new and outside the checkout. The script installs the
wheel and npm archive offline, generates the source separately, and calls only
ordinary installed `infuse`/`carve` interfaces. No source credentials, private
application imports or native-library overrides are used. The probe is a local
process test, not the separate namespace-isolation qualification.
