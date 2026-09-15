# Retained preparation and storage costs

This is an object-level reconciliation for the frozen d15 main programme and its separate coalescing sentinel. No conversion, source acquisition, index build or query was run for this note. Current serving uses source commit `d15b0bccdf87654e6e6bd221ce114fb4f129e42d`; every preparation time below predates it. The recipe and source bytes were preserved, not regenerated with d15.

## Main objects and retained originals

The CSV contains **38 distinct object paths**: all **33 main serving objects** (336,407,132 logical file bytes), two retained ordinary TIFF inputs not selected as main serving controls, and three distinct sentinel SKVs. It matches every main `objects` path, SHA256 and byte length. The CSV is an object inventory, not a sum over 588 operations or 196 cell/lane slots. Logical file bytes are not filesystem allocation and do not include packaging, source checkouts or evidence.

| Dataset | Original ordinary TIFF bytes | SKV bytes | Both retained bytes | Historical complete SKV build, s |
|---|---:|---:|---:|---:|
| analytical36 | 4,464,831 | 5,770,145 | 10,234,976 | 1.103089 |
| analytical40-1025x1031 | 17,357,747 | 24,731,327 | 42,089,074 | 6.005890 |
| real-age36 | 27,955,382 | 28,933,939 | 56,889,321 | 1.494648 |
| worldpop1 | 18,084,641 | 22,778,369 | 40,863,010 | 44.257378 |

The four ordinary compile inputs total 67,862,601 bytes; their four SKVs total 82,213,780 bytes; retaining both totals 150,076,381 bytes. These are the same originals and SKVs across summary-on/off lanes. SKV serving is self-contained (`serving_requires_original=false`); original retention supports provenance and rebuilding. “Original” here means the exact normalized ordinary TIFF compile input, not every earlier download or a larger private country source. The 40-band dataset is generated, not actual real40.

The complete conversion costs match the exact-byte historical lifecycle rows. The main SKV writer library was `1f9e848402532f61eaf503e18230e0be89474741f5e196b0389d45e0ca992f3e`, recorded in the original preparation manifests. This is distinct from the later R11 serving library `5b6fcdf1…60639`, its build commit `507a6e9…`, the later source export `f54783e…`, and d15. No source commit is inferred from an unlinked library hash. Any changed pixels, masks, metadata or layout require a full immutable rebuild; no incremental update saving is claimed.

The CSV also retains available upstream fixture-preparation durations: analytical generation, authorized real36 crop/encoding, and existing WorldPop ordinary encoding. They have different boundaries from downstream SKV compilation and are separate stages, not missing conversion overhead. Original lifecycle CSV blanks remain unknown at that table’s encoding grain. These one-off historical measurements do not predict OS-cold rebuild latency, network acquisition, every input, or another machine.

## Controls and indexes

Direct control costs are joined by exact dataset, variant, source path and SHA256. This includes controls selected by the retained R12 evidence that were absent from some original 672 lanes. BAND256 is a tiled GTiff, not a COG. PIXEL128/256 are GDAL COG layouts; BAND128 COG carries its explicit Rasterio 1.5.1 / bundled GDAL 3.12.4 encoder provenance. Other retained fixture encoding uses GDAL 3.8.4. Their recorded `elapsed_seconds` is the historical rearrangement/encoding cost; d15 did not produce new competing objects. Selection is fastest observed eligible within the declared tested control class on old geometry, not a universal optimum. Native and natural exactextract selections remain separate policies.

| Dataset | Control / index edge | COG bytes | All prepared RSI bytes | Recorded all-group build, s |
|---|---|---:|---:|---:|
| analytical36 | cogband128 / index64 | 6,142,678 | 311,552 | 0.521216 |
| analytical36 | cogband128 / index256 | 6,142,678 | 152,128 | 0.237708 |
| analytical40-1025x1031 | cogband128 / index64 | 26,442,685 | 811,648 | 2.205836 |
| analytical40-1025x1031 | cogband128 / index256 | 26,442,685 | 195,968 | 0.864407 |
| real-age36 | cog256 / index256 | 21,256,336 | 138,592 | 0.296377 |
| real-age36 | cogband128 / index64 | 27,889,906 | 258,912 | 0.549710 |
| real-age36 | cogband128 / index256 | 27,889,906 | 138,592 | 0.241160 |
| worldpop1 | cogband128 / index64 | 18,648,801 | 1,673,224 | 3.116000 |
| worldpop1 | cogband128 / index256 | 18,648,801 | 168,424 | 0.924781 |

RSI is additional to its exact boundary TIFF/COG, not a replacement for it. Existing indexes contain at most 20 bands per group. A band 23 query needs only the group starting at 20; full 36/40-band queries use both groups in one job. The CSV lists each physical RSI once and separately repeats its all-group bundle total for context: **do not add that repeated total across rows**. Nor should COG bytes be multiplied by index groups, transports or repetitions. Generated40 RSI controls were built with library `c8a38d1e102541f6f7b9e92e4cfc2cf37d5eeb2a22e620ca10a511823598d139`; other listed RSI builders use `1f9e8484…992f3e`. Index preparation is additional to producing the chosen TIFF/COG, but per-group subphases are already inside their complete bundle time.

## Separate sentinel layouts

| Dataset / layout | SKV bytes | Historical source-through-compile, s | Writer |
|---|---:|---:|---|
| analytical40-1025x1031 / R12-band-group1 | 24,731,327 | 5.473502 | `5b6fcdf1696b…` |
| real-age36 / R12-band-group1 | 28,933,939 | 1.519653 | `5b6fcdf1696b…` |
| analytical40-1025x1031 / R8-row-group40 | 43,567,822 | 7.031308 | `a50601aa6bcf…` |

The R12 group1 objects retain encoded samples and summaries while changing physical order; their exact hashes differ from main. Their historical plan pins `5b6fcdf1…60639` and the f54783e source export. The row-group40 object is an R8 historical explicit-development-library compilation (`a50601aa…ade64d`), not an installed d15 writer measurement. Its source-to-complete time is 7.031308 s; full recorded lifecycle including engine setup is 7.052697 s. R12 outer result wall times include independent correctness/oracle work; those envelopes are not added to the conversion clock.

## Accounting boundaries and reproducibility

- Source read/decode, predictor, compression and summary milliseconds are components of complete compilation, not additional costs. Instrumented phases can overlap; no sum is used to manufacture an unexplained remainder.
- `source_retained_bytes` is the compiler’s live source-adapter retained buffer (16MiB here), not source-file disk retention. The CSV renames it `source_retained_buffer_bytes`. `working_bound_bytes` is a bound, `peak_scratch_file_bytes` is a scratch-file measurement, and neither is process RSS. Do not add unrelated peaks as simultaneous memory.
- Encoded payload, directory and bootstrap are components of the SKV file, not extra permanent bytes. Typed sample and mask bytes describe logical uncompressed content, not additional retained files.
- Full rebuild costs are historical; modern reader/reducer/coalescing changes do not prove modern writer speed. No cloud acquisition cost, remote egress tariff, amortization threshold or OS-cold claim is inferred.
- Object checks here use the already admitted source descriptors and original small receipts; no raster payload was read again. Release-package verification is separate in PRIVATE_INSTALLATION_01.md.

Evidence links (each CSV row pins its applicable receipt SHA256):

- [Main freeze](REPORT.md) — SHA256 `c450e98a310a38d7d4621a9656ae444419828934e78f9596789634354890e275`.
- [Prepared source descriptors](REPORT.md) — SHA256 `4eddf525ce10d591e3f2ba2f2eba0b3527b4b4cda5746b26e9cb8ba5c2c5dbb9`.
- [Historical lifecycle table](REPORT.md) — SHA256 `9814fe485a3df025c889f559dd78de10b093190848097abc43246a50fd9247d3`.
- [R12 control map](REPORT.md).
- [Object-level CSV](REPORT.md) — SHA256 `1cfd2d132f93f60e82e9be2ad68fde077b72efe59bea1475aec995a8d5298e06`.
- [prepared-final-other-r3 preparation](REPORT.md) — SHA256 `c540147d452cafa03103540945ff3ecb4de8d949de48724f35b5a08c72501624`.
- [prepared-r3-large40-group40 preparation](REPORT.md) — SHA256 `407b94d81824b3e8d737135417f47f85a831b08f14ad4d752d2c36ecbe37bd5a`.
- [prepared-large40-index-controls preparation](REPORT.md) — SHA256 `ba3241ce4c45dec42cb6a702144e2247a59623b959df93c1dc947fe7116e21f1`.
- [R12-band-group1 / analytical40-1025x1031 receipt](REPORT.md) — SHA256 `613ff91215f6b27cefc0b1a5e7eefe70f22b5be4c6d8a02877dc25d4661b2642`.
- [R12-band-group1 / real-age36 receipt](REPORT.md) — SHA256 `bf98272ef03da1bf5c22dd3a03bdb25de6b3c8acc5c9d160f7130f717d3735cd`.
- [R8-row-group40 / analytical40-1025x1031 receipt](REPORT.md) — SHA256 `22b2a3ba352844b5af244131e3e21f25f2efd17a800b02666e4c540ccfe7af93`.

The authoritative [revision-02 lane-storage table](REPORT.md) has 196 frozen cell/lane rows. An independent cross-check matched all 196 source path/SHA/byte triples, every query-required RSI byte sum, and all 33 object identities to this inventory. Its corrected RSI accounting supersedes the preserved first derivation; no measurement changed. SHA256 `ee146c84b4462bc85560fe29077564c74efe36919d2d7e628639844e6226f319`. Per-object CPU/RSS and source acquisition costs are unavailable where the historical receipt exposes only whole-preparation resource envelopes; those envelopes were not allocated artificially to rows.
