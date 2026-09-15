# Inspect and reproduce the final native benchmark

The published tables are **saved observations of d15b0bc**, not measurements of the newer launch candidate. The original seven-lane programme contains 588 complete operations:14 dataset/query quadrants ×2 transports ×7 lanes ×3 observations. See FROZEN_PROGRAMME.json for the exact seed10738291, geometry, control choices, numerical policy, resource limits, source hashes and runtime versions. The old eight-lane672-operation tournament is a different historical programme.

## Recheck the report without raster access

```sh
python benchmarks/native-skv/verify.py
python benchmarks/native-skv/render.py --root benchmarks/native-skv \
  --output /tmp/skarve-charts-new
```

Rendering needs Matplotlib3.11.1 and NumPy2.5.3. Output must be new. Included PNG/SVG charts are byte-identical exports of the final chart set; regenerated image bytes can differ with renderer metadata/fonts. The numerical tables and generation script identify their inputs. All588 operation records,196 lane summaries,28 fixed-summary comparisons, source/index storage, numerical differences, summary ablations and60 sentinel observations are available under data/ and sentinel/. No timing is rerun by these commands.

`PROVENANCE.json` maps each public evidence file to its retained source hash. Private absolute file references became stable logical identifiers; **private-reference/... is not a downloadable dataset**. Timing, result, cost and loss fields remain unchanged. The raw private trace archive and source rasters are not redistributed. Tests and generated fixtures contain no production data.

## Replicate using public synthetic data

Use the qualified Ubuntu24.04 toolchain with GDAL3.8.4 for ordinary fixtures, and the explicitly separate Rasterio1.5.1/GDAL3.12.4 COG encoder. Install the candidate Python package and test/benchmark dependencies first. `/usr/bin/python3` needs the distribution's `python3-gdal` and `python3-numpy`; the active `python` needs the installed Skarve package, NumPy2.5.3, Rasterio1.5.1, exactextract0.3.0 and Shapely. Keep BLAS/GDAL worker counts at1. No cloud credentials or data download is needed.

```sh
WORK=/tmp/skarve-native-replication-new
mkdir "$WORK"
/usr/bin/python3 benchmarks/skv-v0/fixtures.py --output "$WORK/fixtures36" \
  --bands 36 --width 513 --height 519 --seed 17391
/usr/bin/python3 benchmarks/skv-v0/fixtures.py --output "$WORK/fixtures40" \
  --bands 40 --width 1025 --height 1031 --seed 17391
python benchmarks/skv-v0/modern_cog.py --fixtures "$WORK/fixtures36/fixtures.json" \
  --output "$WORK/fixtures36/modern.json" --block 128
python benchmarks/skv-v0/modern_cog.py --fixtures "$WORK/fixtures40/fixtures.json" \
  --output "$WORK/fixtures40/modern.json" --block 128
LIBRARY=$(python -c 'from raster_engine_lab import resolve_library; print(resolve_library())')
python benchmarks/skv-v0/prepare.py \
  --fixtures "$WORK/fixtures36/modern.json" "$WORK/fixtures40/modern.json" \
  --output "$WORK/prepared" --library "$LIBRARY" \
  --chunk-edge 128 --band-group 64 --predictor byte_delta_v1 \
  --payload-layout band --skip-equivalent-cog --extra-index-variants cogband128
python benchmarks/native-skv/replicate.py freeze \
  --prepared "$WORK/prepared/prepared.json" --output "$WORK/final"
# Copy the exact freeze_sha256 printed above. Do not change it after timing begins.
python benchmarks/native-skv/replicate.py run --output "$WORK/final" \
  --freeze-sha256 PRINTED_SHA256
```

These two analytical families produce336 operations with the same four query quadrants, local/HTTP controls and fixed geometry. The new replication records its own actual object/runtime hashes. Source logical sample/mask hashes, interpretation and predeclared layouts must match; it refuses silent substitution. Preparation receives no polygons. `--smoke --datasets analytical36` at the freeze step creates exactly two local functional operations, explicitly excluded from full benchmark claims. It is useful for testing the installed reproduction harness before a long independent run.

The runner executes the original final native worker, numerical validators and HTTP server. Source open, identity, metadata, indexes, ranges, decoding, computation and consumed five-field output remain inside primary timing. Imports/runtime verification/engine lifecycle are reported separately. Fresh processes do not evict the OS page cache; pre-run file hashing warms it. HTTP is unthrottled loopback, not WAN/R2. Do not run simultaneous builds or other benchmarks. The fixed2GiB process address-space,1GiB job working budget,64MiB caches,180second timeout and source request/byte bounds remain unchanged. Complete-operation failures stay in the denominator; there is no retry or outcome-selected layout.

## Real data and exact historical reproduction

Full588-operation replication additionally requires the exact real36 native crop and WorldPop source under their applicable access/data rights. No acquisition URLs or source pixels are included. Their logical hashes, native grids, query frame and physical layouts are in FROZEN_PROGRAMME.json. Supply their independently prepared manifests and explicitly include `real-age36,worldpop1` in `--datasets`; source validation is identical. Generated40 is **not** a real40 result.

The old d15 binary hashes and dependency versions identify the measured implementation. A rebuilt native library or newly prepared object has a new identity even if logical values agree; new timings are replication results, never replacements for the saved report. The launch facade does not silently inherit benchmark results. Historical conversion times refer to the unchanged original objects/writer; newly executed preparation writes its own timing and storage receipts.

The independent sentinel against the older b7 build preserves batch regressions in the saved public tables. It is context, not a requirement to rebuild an obsolete runtime for every public installation. No new optimization campaign is part of reproduction.
