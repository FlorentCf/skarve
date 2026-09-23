# Supported scope and limitations

- Qualified artifacts: Ubuntu 24.04, Linux x86-64, system GDAL 3.8.4. These are
  not manylinux wheels. Windows/macOS binaries and other hardware are unqualified.
- Supported grids are axis-aligned, north-up and queried without reprojection or
  resampling. The polygon must already use the declared source CRS.
- Local GeoTIFF, the explicit bounded HTTP COG contract, and the selected CF
  scientific-grid envelope are reader contracts, not every format GDAL can open.
  See the precise [capability manifest](capability-manifest.json).
- Experimental SKV v0 adds a self-contained optional snapshot for the declared
  typed TIFF/COG contract. It preserves raw sample/mask bits and exposed metadata,
  with 64 stored bands and 128 GiB per object. The limit is 131,072 typed
  leaf records for independent payloads, or 16,777,216 for ordered grouped
  payloads. It does not
  reconstruct arbitrary TIFF tags. See [SKV](skv.md) for limits and versioning.
- Invalid geometry is rejected, not repaired. Difficult positive thin native
  intersections retain strict safeguards; optional exactextract has its own
  declared fractional policy and known finite-precision differences.
- Tracked cache/window/output budgets do not bound every GDAL/GEOS/allocator
  allocation. Embedded execution is cooperative, without a hard kill guarantee.
- Optional backend operation/source/index combinations are finite. Unsupported
  statistics, policies or execution envelopes fail; integer counts and spherical
  semantics cannot be inferred from fractional support.
- Ordinary native jobs run serially per session. Required-range HTTP preparation
  can opt into two bounded transport workers with `SKARVE_HTTP_CONCURRENCY=2`.
  A separate application scheduler must bound process/session concurrency and
  account for its own memory. See [source-window limits](source-windows.md).

The native direct path stays available without preparation or exactextract.
[SUPPORT.md](SUPPORT.md) describes deployment and identity boundaries;
[COMPATIBILITY.md](COMPATIBILITY.md) describes versioned contracts.
