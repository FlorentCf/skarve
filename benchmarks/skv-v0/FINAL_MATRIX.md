# Finite final comparison

Status: **accepted 672-job design; runtime, recipe and fresh geometry are not yet frozen**. The actual-application ordered-policy programme is a separate private evidence track. Its runtime and routing gates precede the final freeze. This file describes the general fractional-query comparison.

The earlier unrun 1,024-job proposal is preserved verbatim in [FINAL_MATRIX_PREVIOUS_1024.md](FINAL_MATRIX_PREVIOUS_1024.md). This finite programme supersedes it. Completed development observations, including every failure and storage, transfer or latency loss, remain historical evidence.

## Four data/query cases

| Dataset | Serving extent | Cases |
|---|---|---|
| Public analytical 36-band fixture | 513 × 519; 36 distinct generated bands | A, B, C, D |
| Public analytical 40-band fixture | 1,025 × 1,031; 40 distinct generated bands | A, B, C, D |
| Real demographics fixture | One exact native 512 × 512 crop; 36 actual bands | A, B, C, D |
| WorldPop population | Complete 9,722 × 7,019 one-band object; declared query window | A, B |

A means one polygon and one band; B means multiple polygons and one band; C means one polygon and all bands; D means multiple polygons and all bands. Generated 40-band data is never described as a real 40-band source. The real native demographics crop is distinct from the separately retained application overview crop.

Each of these 14 dataset/quadrants runs locally and over controlled HTTP with zero added delay, using three fresh process/source lifecycles and predetermined geometry variants. Eight lanes give **14 × 2 × 3 × 8 = 672 operations**.

WorldPop remains a complete 68,238,718-cell serving object. Its bounded cohort uses a declared 1,024 × 1,024 query window. For that dataset, `edge` and `outside` family names refer to the query window; they do not imply source-outside or empty support. Other fixtures and strict correctness tests exercise actual raster edges and outside support.

## Eight complete-operation lanes

1. Native ordinary TIFF.
2. Native tuned COG, using the predeclared applicable layout below.
3. Native COG with the existing RSI64 summary index.
4. Native COG with the existing RSI256 summary index.
5. Native SKV with embedded summaries disabled.
6. Native SKV with summaries enabled, using exactly the same object as lane 5.
7. Explicit optional exactextract over SKV, under the declared eligible strategy.
8. Natural upstream exactextract over the applicable tuned COG, under the same strategy as lane 7.

The general SKV recipe retains per-band payloads, chunk edge 128, band group equal to the source's band count, byte-delta predictor and DEFLATE level 3. The optional grouped payload candidate remains a separate measured ablation: it improved the correlated application workload but increased storage and lost all four general development cases to the existing per-band layout. It is not silently substituted into this primary programme.

| Dataset/case | Native COG | COG with RSI64 / RSI256 | Natural upstream COG | Exactextract strategy |
|---|---|---|---|---|
| Analytical fixtures; WorldPop | BAND128 | BAND128 | BAND128 | Feature for A/C; raster for B/D |
| Real36 A/B | BAND128 | BAND128 | BAND128 | Feature for A; raster for B |
| Real36 C | PIXEL128 | BAND128 / BAND128 local, PIXEL256 HTTP0 | PIXEL128 | Feature |
| Real36 D | PIXEL128 | BAND128 / BAND128 local, PIXEL256 HTTP0 | PIXEL256 | Raster |

These choices precede final geometry generation. The retained large40 comparison found BAND128 stronger than TILE128 under both upstream strategies. The retained real36 qualification found PIXEL layouts stronger for its all-band cases. Encoder versions, compression options and reader versions remain attached to each fixture and programme; layouts do not imply identical encoders. These observations support the named controls, not a universal layout claim. Both existing RSI edges remain measured; a post-hoc best-index envelope must be labelled as such.

An additional16-job existing-geometry real36 check established stronger prepared controls before final freeze: RSI64 uses BAND128 in both regimes; RSI256 uses BAND128 locally and PIXEL256 over HTTP0. All16 operations pass the strict numerical pairing, including comparison with the earlier admitted unprepared COG answers. The historical weaker PIXEL256+RSI64 observations remain preserved. `final_programme.py` expresses these choices in28 dataset/quadrant/regime blocks, each with24 jobs; the total remains672. Every block freezes before any final query timing.

For 36/40-band RSI workloads, the historical format requires two source/index groups with at most 20 bands per index. Both opens and all remote index bytes are inside the complete-operation clock. Its existing cap is not raised for this comparison.

The retained indexed controls use128MiB read and tile admission bounds. Ordinary single-polygon `carve` retains its64MiB read bound, and unindexed batch retains256MiB tile admission. These are explicit existing operation limits, not a claim that every lane has the same internal allocation. The process cap remains2GiB and every admission failure remains visible.

## Numerical and resource accounting

Native results compare against the admitted tuned-COG native lane for the identical dataset, quadrant, polygon, regime and repeat. Optional exactextract results compare against natural upstream using the identical declared strategy and policy. Cross-policy numerical differences are reported separately. Existing tolerances and result definitions remain unchanged.

Ordinary-TIFF default read-admission failures and optional-backend request/byte failures remain unsuccessful operations. They are included in the workload denominator and are not recorded as fast successful queries. A failed numerical reference leaves its pairing unestablished. No failed operation is retried under a larger resource cap to manufacture a win.

Every operation starts before source opening and ends after the complete result is consumed. The primary timer includes metadata, source verification, indexes, requests, decoding, computation and output serialization. Process startup, engine creation and teardown are also reported as separate inclusive lifecycle costs. No two timed jobs share a source handle, engine, decoded cache or index handle.

Once-per-programme source/index hashing is measured and disclosed as setup; it can warm the OS cache. Engine identity work remains inside each primary timer. These are source-cold trials, not disk-cold or production-WAN claims. Qualified dynamic native dependencies are pinned by resolved path and SHA alongside the immutable engine and installed bindings. Existing process, source, index, request, byte and cache limits in [PROTOCOL.md](PROTOCOL.md) remain in force.

The final freeze pins accepted runtime, installed artifacts, fixture identities, exact lane recipe, code and seed before fresh polygon generation. Later corrections require a new immutable programme. The previously proposed broad delayed matrix and whole-country diagnostics are not represented as completed final results; completed focused latency observations remain separately identified development evidence.
