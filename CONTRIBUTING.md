# Contributing

Use the public Skarve repository for release code after its initial publication.
Keep experiments separate until a bounded comparison justifies their integration.
Proposals should identify the input contract, the avoidable work and a measurable
acceptance gate. Established prior art is welcome; novelty is not an acceptance criterion.

Install Rust from `rust-toolchain.toml`, Ubuntu `libgdal-dev`, Python 3.12 and Node
20 or later. Install `scripts/packaging-requirements.txt` and
`scripts/test-requirements.txt` in a virtual environment, and run
`npm ci --ignore-scripts --prefix bindings/node`.

```sh
python scripts/build.py --test
python -m pytest -q tests
node --test bindings/node/*.test.mjs bindings/node/test.mjs
```

Ordinary source tests use the debug native core built by Cargo. For release
qualification, build and package a clean committed tree, then run
`tests/standalone_suite.py` against the resulting archives outside the checkout.
The CI workflow contains the same commands. Benchmark smoke validates outputs
and bounded completion; runner noise is not a reason to enforce a millisecond ranking.

Submit a small PR explaining the changed behavior, tests and numerical or
resource implications. Preserve source identity checks, explicit policy names,
thin-geometry cases, negative evidence and third-party attribution. Do not commit
credentials, signed URLs, production identifiers or unlicensed data. Use generated
fixtures and declare their origin. Contributions intended for inclusion are
submitted under the project's Apache-2.0 license; identify any third-party material
and its separate terms. There is no response-time or support SLA.
