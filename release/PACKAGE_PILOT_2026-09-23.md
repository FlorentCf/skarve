# Installation pilot: Ubuntu 24.04

This is a local-use-only installation test of source commit
`39071592b63c0c9e4f3270df23d22cf9189849f5` (`0.1.1-alpha.3`). It does
not approve or publish PyPI, npm, crates.io, a container, or downloadable
binaries. The binary-distribution gate in [DISTRIBUTION.md](DISTRIBUTION.md)
remains open.

## Host and inputs

- One fresh DigitalOcean Ubuntu 24.04.4 LTS x86-64 droplet in Frankfurt, Basic
  regular shared CPU, 2 vCPU, 4 GB RAM, 80 GB SSD. No pre-existing GDAL,
  Node, Rust, project checkout, or Skarve installation.
- Python 3.12.3; Node 22.23.2/npm 10.9.8 from the official Node archive,
  SHA-256 checked; Rust/Cargo 1.98.1 via official rustup.
- Ubuntu packages `libgdal34t64` and `gdal-data`
  `3.8.4+dfsg-3ubuntu3`, `libdeflate0` `1.19-1build1.1`; Rust build also
  used `libgdal-dev`, `build-essential`, and `pkg-config`.
- The complete source archive and local-use-only wheel, npm archive, and CLI
  archive were generated on a separate Ubuntu 24.04 WSL host. They were
  transferred to the droplet and all four passed `sha256sum -c SHA256SUMS`
  before installation. The separate offline test-dependency wheelhouse was
  also transferred. No remote source repository or private application was
  mounted or cloned.

| Candidate archive | SHA-256 |
| --- | --- |
| Python wheel | `2d6262d0834fc50677d1911f288f9d87a531846c63be9b4bf4698813921d687f` |
| npm archive | `d0de75ae7bc325e1476a307784942eb474df4f7d24a28c3c48504ae9f60fb669` |
| CLI archive | `7b944efa7ac29370e65bc1b8ff244cb2b3fa6e8142e64d115c0abf574998cf7a` |
| source archive | `543329bbebf4e717f3cad69e0c041eb71b673b0a66f73866ae3b7b34e6c88db0` |

## Results

1. Before installing GDAL, the CLI exited with an explicit missing-runtime
   message and the pinned Ubuntu `apt-get` command. After installing the
   documented packages, `skarve --version` and `skarve backends` succeeded;
   native was available and optional exactextract was absent, as expected.
2. `tests/standalone_suite.py` installed the wheel and npm archive offline
   and extracted the CLI archive in a fresh consumer directory. All 58
   checks passed with zero new external network requests. They cover source
   windows and masks against an independent rasterio oracle, CLI/Python/Node
   operations, batch results, and generated COG and SKV workflows. Full receipt SHA-256:
   `76e0fda2a4edd50ff020fc593d789877eaaf447b6e59fa918bd598a45b6e973a`.
3. `scripts/test-rust-package.py` packaged the crate and built an external
   consumer from the extracted `.crate`, without the source checkout. The
   consumer returned two rows and sums `[36.0, 72.0]`, and queried its SKV
   after removing the original raster. Packaged API tests and doc tests
   passed. Full receipt SHA-256:
   `85d7433449f47469a2fe1b1ca02902221c066b519dd61d182c998bb283d25a0e`.
4. `cargo install --path` from the extracted crate succeeded and its installed
   executable reported the expected version. Its executable name is
   `raster-engine`, while the wheel and CLI archive expose `skarve`.

## Findings and next decisions

- The documented offline test wheelhouse command omitted `blake3==1.0.8`,
  which the suite requires. The command is corrected in `docs/INSTALL.md`.
- The Linux archives depend on the exact GDAL/libdeflate runtime above; they
  are not standalone or portable across arbitrary systems. An ordinary
  `pip install skarve-engine`, `npm install @skarve/engine`, or
  `cargo add skarve` cannot be offered yet because the registries do not have
  these packages and publication remains gated.
- A targeted content audit of the current source tree, source archive, wheel,
  and CLI archive passed. The npm archive failed that audit because its bundled
  third-party Koffi native binaries contain a vendor build-home path. A
  history-inclusive audit also flagged older experiment files with private
  path patterns. Both require precise review before public binary publication;
  this pilot neither suppresses the rules nor treats those findings as proof
  of a leaked credential.
- For eventual public installation, resolve the binary license/dependency
  decision first, then review the npm audit finding, agree on package names
  and supported runtime matrix, and test registry-installed artifacts on
  clean hosts. Consider aligning the Rust-installed CLI name with `skarve`.
  Keep benchmark and release harness material out of the consumer API.

These results cover one Ubuntu host and one generated-data workload. They do
not establish Windows/macOS support, public registry behavior, legal clearance,
or production-scale performance.
