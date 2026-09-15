<!-- Archived unrun design; superseded by FINAL_MATRIX.md. No outcomes are implied. -->

# Finite final comparison

Status: reserved baseline, not yet frozen or executed. The subsequent HM-focused continuation requires its actual numerical/source/workload contract and policy-preserving routing gates before the final freeze. A 48-job real36 control qualification also establishes that PIXEL COG can be stronger for its all-band cases; those applicable controls must be retained in a declared supplement or dataset-specific comparison. The 1024 jobs below remain the baseline, not an assertion that this is the final expanded total.

This matrix is fixed after development qualification and before fresh final geometry seeds. Format options and the installed library are pinned in each generated `freeze.json`. It replaces repetition of every weak development control with the strongest applicable controls and a small latency sensitivity programme. All development results remain separate, including failures and storage/transfer losses.

## Primary matrix: 924 complete operations

Datasets and cases:

| Dataset | Extent | Cases |
|---|---|---|
| Public analytical 36-band fixture | 513 by 519, 36 distinct bands | A, B, C, D |
| Public analytical 40-band fixture | 1025 by 1031, 40 distinct bands | A, B, C, D |
| Real demographics fixture | One native 512 by 512 tile, 36 actual bands | A, B, C, D |
| WorldPop population | Complete native 9722 by 7019 one-band object; predeclared query window | A, B |

Each of these 14 quadrants runs locally and over controlled HTTP with zero added delay, for three fresh process/source lifecycles and three predetermined geometry variants. Eleven lanes give 14 × 2 × 3 × 11 = 924 jobs:

For complete WorldPop, `edge` and `outside` geometry-family labels refer to the predeclared query window. The complete raster can contain valid values there; those labels do not assert source-outside or empty support. The other serving fixtures and strict correctness tests exercise actual source edges and outside support.

1. Native ordinary TIFF.
2. Native modern BAND128 COG.
3. Native modern BAND128 COG plus existing RSI64 summaries.
4. Native modern BAND128 COG plus existing RSI256 summaries.
5. Native SKV with embedded summaries disabled.
6. Native SKV with summaries enabled, using exactly the same object as lane 5.
7. Natural upstream exactextract over ordinary TIFF, feature strategy.
8. Natural upstream exactextract over ordinary TIFF, raster strategy.
9. Natural upstream exactextract over modern BAND128 COG, feature strategy.
10. Natural upstream exactextract over modern BAND128 COG, raster strategy.
11. Explicit installed exactextract-over-SKV, raster strategy, summaries disabled.

Modern BAND128 was selected before the final freeze by a 16-job HTTP development check against modern TILE128. BAND was faster for all four cases under both natural upstream strategies on the fixed large40 cohort. Both layouts' complete outcomes remain available; this selection is evidence for the named cohort, not a universal layout claim. COG and ordinary-TIFF raw pixels and interpretation must match independently. Keep both RSI edges; any post-hoc best-index envelope is labelled as such.

Native comparisons declare native modern BAND128 as their numerical reference. Each exactextract strategy declares natural upstream ordinary TIFF using the matching strategy as its reference. These are explicit numerical-policy comparisons with unchanged existing tolerances. Native and exactextract policies are reported separately. The development programme already preserves exactextract-over-SKV feature-strategy eligibility failures and successful alternatives; it is not retried with a higher request cap for this final matrix.

## Latency sensitivity: 98 complete operations

The large40 fixture runs A/B/C/D through SKV raw, SKV summaries, COG+RSI64, COG+RSI256, natural upstream BAND128 feature/raster and exactextract-over-SKV raster at 8 ms per request and 64 MiB/s, with three fresh lifecycles: 84 jobs. A/D repeat the same seven lanes once at 25 ms and 16 MiB/s: 14 descriptive jobs. The single-repeat 25 ms results are not a distribution or p95 estimate. These strong natural controls are mandatory: their observed development request count can be lower than native source/index access, which is material under added latency. Numerical references for these focused regimes are native COG+RSI64 and natural upstream BAND128 with the matching exactextract strategy, explicitly pinned before execution.

## Whole-source diagnostics: two complete operations

Two separately labelled complete-WorldPop large-interior queries use SKV summaries and the predeclared modern COG+RSI64 control. These diagnose genuinely large covered interiors and are excluded from the bounded-window main matrix. They do not represent normal query latency.

The finite total is 1024 jobs. Failed operations are retained with exact resource or numerical reasons; a failed declared control leaves that pairing unestablished. A later correction receives a new immutable programme, rather than rewriting the original result.

No timed job shares an engine, source handle, decoded cache or index handle with another job. OS cache is uncontrolled and explicitly disclosed. Source/index integrity setup is measured once outside the query programme; engine verification and every needed source/index request remain inside the before-open to consumed-result boundary. Process startup and teardown are separately reported inclusive costs. All existing process, source, index, request, byte and cache limits in `PROTOCOL.md` remain in force. No cloud result is claimed from loopback modelling.
