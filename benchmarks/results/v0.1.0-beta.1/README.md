# Public synthetic confirmation

This directory contains newly generated, credential-free measurements of the installed `skarve-engine` 0.1.0b1 wheel and `@skarve/engine` 0.1.0-beta.1 npm artifact. It contains no raster binaries or application fixtures. The generator recreates all eight source rasters from the recorded seed. The report is in [../../report/index.html](../../report/index.html).

Selected native SHA-256: `17e3546f7c526d5687b99641ce3f3adaa6d8d85cd7e4786fc84b2e0d2bf78c93`.

The Python and Node dependencies were installed from the built artifacts into separate clean consumer directories. Computation used the installed package imports and bundled native libraries, with no source-binding override. The freeze records the benchmark script hash and actual installed library hash. The artifact checksums used were:

- Wheel: `b9ae8b9decf7b8b3031aeaddaacba8b896e6bbe24542c23e2c7e3af33bdd4ad3`.
- npm tarball: `fad4363dcff9da28abf2919ece11dc53868a0ef219722d259a1600c808a7d104`.

These were prepublication development artifacts. Later documentation-only rebuilds may change archive hashes while retaining the same verified native library. Match the native hash and source manifest; do not assume archive byte identity.

After installing the matching packages and benchmark requirements, reproduce inputs and complete calls with:

```sh
python benchmarks/quickbench.py \
  --expected-library-sha256 17e3546f7c526d5687b99641ce3f3adaa6d8d85cd7e4786fc84b2e0d2bf78c93 \
  --seed 193542725648087 --output scratch/public-replay
node benchmarks/bulk_quickbench.mjs \
  --expected-library-sha256 17e3546f7c526d5687b99641ce3f3adaa6d8d85cd7e4786fc84b2e0d2bf78c93 \
  --seed 2179017736 --output scratch/public-replay/bulk.json
python benchmarks/report.py --input scratch/public-replay \
  --bulk scratch/public-replay/bulk.json
```

For a new confirmation cohort, omit both seeds and use a new output directory. Timing is host-dependent. This run used one serial worker under WSL2 on an AMD Ryzen 7 8745HS, Python 3.12.3 and Node 20.20.2. Native GDAL 3.8.4 and rasterio GDAL 3.12.4 differ; the comparison includes these installed source adapters. Python address space was capped at 2 GiB; Node RSS was observed, with explicitly bounded payload and native owned buffers.

The 864 source/batch/preparation records contain 168,456 native field checks with no mismatches. The 160 typed timing records contain 33,440 exact field checks with no mismatches. Exactextract's 11,786 differences among 71,120 checks are preserved as numerical incompatibility at the declared native tolerance. Repeated checks are not unique polygon counts. All slower native methods remain in the raw data and report.

The initial smoke harness exposed two control-output adaptation issues before the final freeze: the special upstream `id` property, and upstream NaN empty means/extrema. The final harness uses an ordinary `zone_key` and maps empty NaN mean/min/max to null only at zero support. No nonempty numbers or coverage differences were changed. Native code, numerical definition, and tolerances remained frozen.
