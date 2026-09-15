# Runnable workflows

Start with [installation](INSTALL.md) and [generated data](getting-started.md).
The examples require no private application or raster credentials. Skarve owns
source reading, decoding, geometry and aggregation; preparation is optional.

| Workflow | Runnable example | Contract |
|---|---|---|
| Original source, optional index, many-zone/date stack | [Python](../examples/python_source.py), [Node](../examples/node_source.mjs) | Native strict, original boundary values, two rows per page |
| Explicit backend, single polygon and batch | [Python](../examples/python_backend.py), [Node](../examples/node_backend.mjs) | Five common metrics, explicit policy/provenance |
| Selected typed values | [Python](../examples/python_bulk.py), [Node](../examples/node_bulk.mjs) | Advanced supplied-selection ABI; no source discovery or polygon coverage |

Use the [API reference](api.md), [Python guide](python.md), [Node guide](node.md)
and [CLI guide](cli.md) for public verbs and compatibility names. Detailed
contracts live in [Backends](backends.md), [Batch](batch.md),
[Preparation](preparation.md), [Numerics](NUMERICS.md) and
[Sources and caching](sources-and-caching.md).

## Remote source and bounded reuse

Pass an authorized HTTPS location through the same `infuse` interface:

```python
spec = {
    'location': 'https://YOUR-AUTHORIZED-HOST/path/source.cog.tif',
    'http': {'max_requests': 128, 'max_range_bytes': 1024 * 1024,
             'max_download_bytes': 8 * 1024 * 1024, 'cache_bytes': 256 * 1024},
}
# with sk.infuse(spec) as source:
#     result = source.carve(zone=polygon, crs='EPSG:4326')
```

The server must support bounded ranges and the stable-validator contract. This
placeholder grants no URL access. An application must enforce its host/address
allowlist, credentials and outbound network boundary. Keep signed URLs and
secrets out of permanent examples and logs. Cache hits can avoid GET bodies
while still issuing metadata HEAD checks; both count toward resource accounting.

[range_server.py](../examples/range_server.py) serves generated COG data on
loopback for the installed consumer suite. `allow_http: true` is used explicitly
for that local test, not as a public remote default.

## Installed consumer checks

[standalone_suite.py](../tests/standalone_suite.py) installs the matching wheel,
Node archive and CLI archive into a fresh consumer, then runs generated-only
examples and focused numerical/resource contracts. It checks source mutation,
index identity, original boundary reads, retained caches, pagination, cleanup,
typed buffers and backend availability. The optional build adds delegated
single/batch checks without an upstream Python runtime dependency.

Use the [offline consumer commands](INSTALL.md#verify-installed-archives) and
retain the resulting artifact hashes and receipt. The suite's environment cleanup
is not a filesystem/network sandbox; only an enclosing isolation receipt can
establish that private mounts were inaccessible. A clean installation on one
host is not a second-hardware replication. Observed losses and lifecycle costs
remain in the [benchmark report](../benchmarks/report/index.html).
