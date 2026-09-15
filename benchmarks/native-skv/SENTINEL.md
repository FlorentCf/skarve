# Final coalescing sentinel: b7 versus integrated d15

All **30 fresh process lifecycles, 60 complete queries and 30 matched exact output pairs pass**. Seven of ten latency medians improve; three regress. All **12 individual latency losses** remain below. This is a whole-build comparison with b7, not an ablation that isolates coalescing from the other integrated changes. No new engine operation, build, test or source-pixel read was performed for this report.

Control is `b7bccd79e62c3d8e87cf5a2a84319207d98f4e19`, installed library SHA-256 `1d1e4d3b0366d3fa769c009c04b936745c67276f16c29ec287bad91fcf4efba6`. Candidate is `d15b0bccdf87654e6e6bd221ce114fb4f129e42d`, library `45cea7221247d815f1cf15ad0fe279ed2dd1b9253b675c033a3954a75eb7b67e`. These are the frozen qualified installed artifacts. The saved records bind them; this analysis does not rerun artifact qualification.

The generated40 raster is **1025 × 1031 with 40 distinct bands**, in two existing physical representations: `band_group=1`, independent-band `band` payloads (24,731,327 B), and `band_group=40`, `row_group_v1` payloads (43,567,822 B). The real36 raster is the authorized **512 × 512 native crop with 36 distinct bands**, independent-band `band_group=1` payloads (28,933,939 B). All have 128-pixel chunks, `byte_delta_v1`, DEFLATE level 3 and stored summaries. This is not genuine real40 evidence. Exact paths, source hashes, logical digests and writer provenance are in the diagnostic JSON.

Five cells each have three matched fresh rounds. Singlewide uses one polygon and ordinary `carve`; mixed uses one ordinary eight-polygon `cleave` per phase. The cold geometry-generation frame is the full source dimension stated above; the distinct retained Q2 frame shifts by (+3.25, −1.5) pixels on the same registered source and engine. These are frames for the frozen polygons, not claims that every query covers the full raster. HTTP5 is 5 ms configured request delay and 64 MiB/s body service; HTTP0 has zero configured delay and the same body rate. The fixed native policy is `native_grid_planar_fractional`, with sum/support/mean/min/max; no mask expression or median/quantile output is requested.

**All latency results.** Milliseconds are source registration/open through fully consumed Q, or distinct retained Q2 through consumption; n=3 per runtime and cell/state. Positive change is a regression. The JSON retains unrounded observations and median/min/max for every reported counter; three observations are not a p95 or a significance test.

| Cell | State | b7 median ms | d15 median ms | Change | Losing pairs |
|---|---|---:|---:|---:|---:|
| Generated40 band singlewide, HTTP5 | Cold | 7,378.302 | 5,621.634 | −23.81% | 0/3 |
| Generated40 band singlewide, HTTP5 | Retained | 7,283.432 | 5,483.826 | −24.71% | 0/3 |
| Generated40 row singlewide, HTTP5 | Cold | 900.368 | 887.297 | −1.45% | 1/3 |
| Generated40 row singlewide, HTTP5 | Retained | 807.397 | 755.156 | −6.47% | 1/3 |
| Real36 band singlewide, HTTP5 | Cold | 3,235.036 | 2,381.422 | −26.39% | 0/3 |
| Real36 band singlewide, HTTP5 | Retained | 55.628 | 58.717 | **+5.55%** | 2/3 |
| Generated40 band mixed batch, HTTP0 | Cold | 2,425.442 | 2,497.802 | **+2.98%** | 3/3 |
| Generated40 band mixed batch, HTTP0 | Retained | 2,385.828 | 2,433.524 | **+2.00%** | 3/3 |
| Generated40 row mixed batch, HTTP0 | Cold | 1,354.390 | 1,327.469 | −1.99% | 1/3 |
| Generated40 row mixed batch, HTTP0 | Retained | 1,287.458 | 1,280.563 | −0.54% | 1/3 |

**Primary CPU and physical transfer.** CPU is measured process CPU during the primary operation, not wall time minus I/O. Body bytes are identical between runtimes in each of the 30 matched pairs. All primary accepted byte intervals are nonrepeating within that operation; retained operations may legitimately reread bytes from Q1.

| Cell/state | b7 → d15 phase CPU ms | b7 → d15 physical requests | Body bytes per runtime |
|---|---:|---:|---:|
| G40 band single, cold | 1,021.806 → 788.404 | 1,131 → 851 | 10,946,276 |
| G40 band single, retained | 967.766 → 710.998 | 1,122 → 842 | 10,402,532 |
| G40 row single, cold | 398.741 → 424.824 | 39 → 32 | 18,960,789 |
| G40 row single, retained | 358.707 → 343.692 | 30 → 23 | 18,417,045 |
| Real36 single, cold | 484.276 → 431.013 | 437 → 293 | 21,941,738 |
| Real36 single, retained | 44.768 → 47.764 | 2 → 2 | 0 |
| G40 band batch, cold | 1,473.329 → 1,518.602 | 2,050 → 2,050 | 18,400,167 |
| G40 band batch, retained | 1,447.280 → 1,468.764 | 2,122 → 2,122 | 18,666,597 |
| G40 row batch, cold | 830.759 → 806.401 | 61 → 61 | 32,456,581 |
| G40 row batch, retained | 747.238 → 738.832 | 55 → 55 | 33,292,354 |

**The band-batch regression is observed with unchanged work volumes.** All 12 batch pairs have identical ordered physical request method/status/range/body sequences. For every pair, raw encoded bytes, decoded bytes, decoder calls, materialized cells, normalized output writes and native output allocation counters match. Cold/retained band batches decode 2,040/2,120 chunks and 157,824,000/164,377,600 raw bytes, writing 284,083,200/295,879,680 normalized bytes. Each uses 51/53 windows. Candidate prefetch calls, admissions, ranges, planning and fetch time are measured zero in all batch phases. Shared-mask passes and scratch are also zero; the requested statistics do not activate median/quantile reuse. These facts exclude extra fetches, extra decoded cells or active prefetch work as an explanation for these particular losses. They do not prove that inactive branches or other whole-build effects are costless.

| Band-batch counter, median ms | Cold b7 → d15 | Retained b7 → d15 |
|---|---:|---:|
| Consumer `read_decode_ms` | 1,968.844 → 2,033.395 | 1,951.650 → 1,992.426 |
| Consumer `read_window_validation_ms` | 19.955 → 24.259 | 21.430 → 23.089 |
| Source `read_ms` | 1,753.162 → 1,777.936 | 1,756.035 → 1,785.455 |
| Source `decode_ms` | 85.265 → 86.917 | 88.725 → 89.321 |
| Source `predictor_decode_ms` | 73.980 → 79.159 | 71.253 → 71.367 |
| Source `normalization_ms` | 59.453 → 65.630 | 58.883 → 59.572 |
| Consumer `reduction_ms` | 346.232 → 346.027 | 330.263 → 333.593 |

The larger measured movement is in the aggregate read/decode side, alongside higher phase CPU and somewhat longer physical server service time. These timers overlap and their medians can come from different rounds; they cannot be summed into a causal decomposition. The saved counters do not identify an individual instruction, allocation, scheduler effect or compiler/code-layout effect. Those remain hypotheses, not diagnoses or optimization recommendations. Row-batch medians improve here, with two individual losses retained; that does not erase earlier batch regressions in other checkpoints.

**Prefetch evidence and the retained real36 loss.** Candidate generated40 band singlewide saves 280 physical requests per phase; row singlewide saves seven per phase; real36 cold saves 144. Those counts equal the candidate's planned request savings. Encoded/decoded work and transfer bytes are unchanged in every matched pair. Candidate source prefetch planning medians are 15.213/0.352 ms for generated40 band cold/retained, 14.831/0.281 ms for row, and 7.682/0 ms for real36. Cold planning is higher here; the saved aggregate does not subdivide that cost. Prefetch fetch time overlaps the reader/consumer timers and must not be added to them as extra latency.

Retained real36 incurs **zero GETs, raw decodes, materialization or new output allocation**, with two conditional guard HEADs in both runtimes. Both retain 63,740,916 decoded-cache bytes and perform the same 12 boundary-tile / 166,788 positive-cell / four summary-tile work. Candidate prefetch calls and queue occupancy are zero; its consumer boundary-planning timer is 0.064 ms. The median loss is +3.089 ms, with primary CPU +2.996 ms. The counters therefore do not support blaming network transfer, decoding or that 0.064 ms planning timer for the full loss. No further causal isolation was performed.

**Lifecycle costs and memory.** These medians include both operations in one process. Wall and process CPU include imports, engine creation, post-query source inspection, close and receipt work. Peak RSS is one process high-water mark, not a per-query allocation count. External HTTP server CPU is not separately measured.

| Cell | b7 → d15 process wall ms | b7 → d15 process CPU ms | b7 → d15 peak RSS MiB |
|---|---:|---:|---:|
| G40 band singlewide | 14,978.160 → 11,421.798 | 2,298.148 → 1,805.872 | 133.211 → 134.320 |
| G40 row singlewide | 1,865.861 → 1,775.801 | 906.259 → 890.576 | 130.184 → 129.641 |
| Real36 singlewide | 3,475.157 → 2,645.468 | 702.229 → 662.476 | 120.246 → 120.184 |
| G40 band mixed | 5,147.179 → 5,266.455 | 3,246.916 → 3,316.063 | 141.688 → 141.938 |
| G40 row mixed | 2,786.022 → 2,767.847 | 1,727.757 → 1,704.875 | 132.500 → 132.145 |

Both runtimes retain zero native raw intermediate allocation. Copy counters are unchanged per pair: independent-band paths report zero `copied_bytes`; grouped-row paths report 91,750,400 B per singlewide phase, and 157,824,000/164,377,600 B for cold/retained batch restoration, with a 2,304 B native row-scratch peak. This is repeated bounded row-scratch copying, not a retained intermediate of that total size. Native normalized outputs remain allocated and charged, as shown above. Maximum observed RSS is 148,865,024 B; encoded-cache occupancy is at most 4 MiB. Candidate prefetch scratch is a reported 8 MiB bound, not measured RSS; peak demanded encoded-cache charge is 4,003,430 B. No cache capacity was enlarged for this comparison.

**Physical accounting and limits.** Recomputed from all completed server timelines: b7 has **21,177 requests** (21,087 GET + 90 HEAD), d15 **19,023** (18,933 GET + 90 HEAD). Each transfers **550,452,237 B**. The 2,154-request reduction is entirely GETs. Each runtime includes 30 post-query inspection HEADs outside primary clocks, zero bytes; those remain in the lifecycle totals. All 30 HTTP traces reconcile with record totals, CSV totals and the final source remote counters. There are zero HTTP errors, incomplete bodies, handler failures or source invalidations. All observed requests are conditional. Maximum lifecycle usage is 4,174 requests and 65,748,935 body bytes; maximum single response is 1,375,742 B. These remain below the frozen 8,192-request, 1 GiB job, 768 MiB source and 4 MiB range limits. Child address space remains 2 GiB, one worker, timeout 120 s.

There are **54 complete logical phase traces and six truncated retained band-batch traces**. Those six retain 4,096 cumulative entries while aggregate logical reads report 4,168. Their logical trace bytes/counts are lower bounds; no complete logical range claim is made from them. Full physical traces are complete in all cases. For the 54 complete logical traces, raw record counts and lengths also match decoder-call and raw-encoded-byte counters. `raw_prefetch` logical bytes overlap later raw reads served from cache; adding both would overstate physical traffic.

**Every individual latency loss**, zero-based frozen round, milliseconds:

| Cell/state | Round | b7 | d15 | Increase |
|---|---:|---:|---:|---:|
| G40 row single/cold | 0 | 900.368 | 924.995 | 24.626 |
| G40 row single/retained | 0 | 852.631 | 877.671 | 25.041 |
| Real36 single/retained | 1 | 72.895 | 77.596 | 4.701 |
| Real36 single/retained | 2 | 53.941 | 58.717 | 4.777 |
| G40 band batch/cold | 0 | 2,446.359 | 2,497.802 | 51.443 |
| G40 band batch/cold | 1 | 2,425.442 | 2,522.010 | 96.568 |
| G40 band batch/cold | 2 | 2,372.302 | 2,406.768 | 34.466 |
| G40 band batch/retained | 0 | 2,410.531 | 2,429.672 | 19.141 |
| G40 band batch/retained | 1 | 2,384.721 | 2,433.524 | 48.803 |
| G40 band batch/retained | 2 | 2,385.828 | 2,438.772 | 52.944 |
| G40 row batch/cold | 1 | 1,285.326 | 1,361.160 | 75.834 |
| G40 row batch/retained | 0 | 1,287.458 | 1,296.137 | 8.678 |

The inputs are [the completed sentinel freeze](REPORT.md), [original audit summary](REPORT.md) and its five hash-bound result files. Freeze SHA-256 is `5b54757c1e822f8216a8bcc3ff34624cfddc2c60bce4a5b9d0a799c0e6bf463f`; complete receipt is `73f89629eaef4c63409013010da47957bfb7d1d1e3115af6b746918665045b36`. [Diagnostic JSON](REPORT.md) has SHA-256 `8c0702bb90779ee58a667e5bd975cda9fd12141fda1098ce2aee54832c4e9cbb` and records all 30 matched deltas, 60 compact operation costs, 30 lifecycle costs, source/runtime pins and saved-input hashes without duplicating full answers. [The stdlib-only derivation helper](REPORT.md) checks raw record/task/runtime identities, recomputes transport partitions, rechecks exact serialized answer bits and reconciles saved medians. It refuses to overwrite its JSON output. Two initial helper-only guard stops (CSV field-size default; treating truncated logical traces as complete) produced no diagnostic output; the correction retains truncation explicitly. Original data, audits and frozen helpers are unchanged.

Fresh process/source lifecycle does **not** mean OS-cache cold: no page-cache eviction is claimed. This finite sentinel does not establish universal performance, WAN/provider behavior, genuine real40 performance or the isolated cost of each integrated change.
