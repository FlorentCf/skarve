# CLI

The installed `skarve` command uses the same native session/source contracts as
Python and Node. `raster-engine` is the native executable compatibility name.

```sh
skarve --version
skarve carve example-data/original.tif example-data/polygon.json \
  --crs EPSG:3857 --bands 0 --metrics sum,support,mean,min,max,count
skarve cleave job.json --max-rows 2
skarve backends
skarve compile example-data/original.tif --output example-data/snapshot.skv \
  --chunk-edge 64 --codec deflate --predictor none
skarve verify-skv example-data/snapshot.skv
```

`job.json` uses the [batch job schema](batch.md); `metrics` and the compatibility
`options: {statistics: [...]}` spelling are alternatives. Commands emit JSON envelopes;
check `ok`, then inspect `result`. Batch output is one envelope per line, with
completion at `result.complete`. Consume each page before accepting the next.
An error is not a partial successful scientific result.

`skarve --help` and subcommand help describe the flags available in the installed
build. [Generated workflows](getting-started.md) require no private credentials.
Backend policy names and cooperative execution limits are common to all callers;
see [Backends](backends.md).

Optional selection uses `--backend exactextract`,
`--numerical-policy exactextract_fractional_v030` or an explicit comma-separated
`--accepted-policies` list. `--execution-envelope embedded_cooperative` states the
execution requirement; `--backend-options` takes a JSON object. For example:

```sh
skarve carve example-data/original.tif example-data/polygon.json \
  --crs EPSG:3857 --bands 0 --metrics sum,support,mean,min,max \
  --backend exactextract --numerical-policy exactextract_fractional_v030
```

`infuse`, `carve`, `ward` and `cleave` have compatibility names `inspect`,
`measure`, `prepare` and `batch`. A one-shot CLI process cannot retain a source
handle after exit; use the Python/Node session for retained operations.

`compile` creates an experimental self-contained SKV; its output must be new.
The original source is unnecessary when serving the snapshot. See [SKV](skv.md)
for optional encoding, summaries, supported raw types and resource limits.

An explicit `--numerical-policy exactextract_rasterio_v030` selects the bounded
[unscaled Rasterio input contract](exactextract-rasterio.md) for the optional
backend. The omitted-policy default remains `exactextract_fractional_v030`.
