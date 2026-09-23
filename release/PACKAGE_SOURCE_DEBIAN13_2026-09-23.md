# Source package pilot on Debian 13

On 2026-09-23, the clean alpha.4 source archives from commit
`d0447ee7f5fd1dba54601c1e23e02de601d8257f` were installed on one new
DigitalOcean Debian 13.5 x86-64 droplet (`603090804`, NYC1, regular shared
2 vCPU/4 GB). The plan was $0.036/hour with no backup or extra volume. This
is a second Linux distribution check following the Ubuntu 24.04 package
pilot; it is not a Windows/macOS or cross-architecture claim.

| Archive | SHA-256 | Clean-host result |
|---|---|---|
| `skarve_engine-0.1.1a4.tar.gz` | `8b91089830db02fc820faaf2d1bd9de470b285f167abe21026009229de43a2f9` | Fresh Python 3.13 venv built wheel and installed; `skarve --version` passed |
| `skarve-engine-0.1.1-alpha.4.tgz` | `9a4a60cd3bcef645bd6a5ff404909560273d83365f380885ad6dae716041a334` | Fresh npm 9/Node 20 install built the native library; installed package reports `@skarve/engine@0.1.1-alpha.4` |
| `skarve-0.1.1-alpha.4.crate` | `fd63bcacbf231a9df933975fa5107584cd73fcba4ec28b70e1a567264383d340` | `cargo install --path` built and installed `raster-engine`; CLI version, backend inspection and TIFF measurement passed |

The three uploaded archives passed `sha256sum -c SHA256SUMS` on the droplet.
The environment used rustc/cargo 1.98.1, Node 20.19.2, Python 3.13.5,
GDAL 3.10.3 and libdeflate 1.23. The installed native library dynamically
loaded system `libgdal.so.36` and `libdeflate.so.0`. Installing system
prerequisites and all three independent source builds took substantially
longer than downloading the approximately 1 MB archives: the Python wheel
build took roughly 4 minutes, npm's native build roughly 3 minutes, and
`cargo install` 4 minutes 14 seconds. These are observations on this shared
2-vCPU VM, not performance guarantees.

The installed Python and Node examples each returned the same direct TIFF
fractional sum `1845.125`, matched direct and summary-indexed results,
completed all 18 paged date-stack rows, and compiled and queried a generated
SKV. Their SKV verifiers reported the same logical digest
`0429a6913fdaa6fe71228c1783adbadd14d9e93a9fdf6435fce6aaf41c85036a`
and served two SKV batch rows without reopening the original source. The
Rust CLI measured the same TIFF band with fractional sum `1845.125`. The
source and archive content audit passed with zero findings, `cargo publish
--dry-run --locked` passed, and the PR's native, exactextract and dependency
review CI jobs passed. No registry upload was attempted.

Practical findings:

- Source-only archives avoid publishing a prebuilt Skarve core, but a new host
  still needs Rust and GDAL/libdeflate development packages and spends minutes
  compiling. Providing a true one-command native install on a bare machine
  would require reviewed binary-distribution obligations and separately
  qualified platform-specific wheels/npm binaries.
- Debian 13's GDAL 3.10.3 and libdeflate 1.23 worked for these installed
  native-only examples. This does not establish all GDAL versions or optional
  exactextract support on Debian.
- The Cargo-installed CLI is named `raster-engine` while Python's command is
  `skarve`. A future compatibility-preserving alias would improve discovery.
- Direct registry commands remain unverified until package names, publisher
  accounts and authentication are established and alpha.4 is actually
  published. Local-archive installs must not be reported as registry installs.

The droplet contained generated synthetic fixtures and disposable build
products only. Existing alpha.3 release artifacts were not changed.

Cleanup: the Debian 13 pilot droplet was destroyed after verification. Its
temporary DigitalOcean SSH key was removed from the account, and the matching
local private key, public key, and known-host files were deleted.
