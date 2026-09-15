# Getting started

Install or build the candidate using [INSTALL.md](INSTALL.md). The source beta
does not imply PyPI/npm registry or prebuilt binary availability. Then, from the
source checkout with its matching packages installed:

```sh
python -m pip install numpy==2.5.3 rasterio==1.5.1
python examples/generate_fixtures.py example-data
python examples/python_source.py --fixture example-data/fixture.json --index python-index
node examples/node_source.mjs example-data/fixture.json node-index
skarve measure example-data/original.tif example-data/polygon.json \
  --crs EPSG:3857 --bands 0 --statistics sum,support,mean,min,max,count
```

Fixture/index directories must be new. Fixture generation uses NumPy/rasterio
only to create public mathematical data; normal file queries need neither.
The examples read original sources, build and reopen an optional summary index,
and consume a two-file date stack in bounded pages. No private credentials or
downloaded raster is needed. Node resolves `@skarve/engine` from the installed
consumer directory.

Learn the small [API](api.md), then choose [Python](python.md), [Node](node.md),
[CLI](cli.md) or [batch](batch.md). Read [backend policies](backends.md) before
requesting optional exactextract execution.
