# Public source package release: 0.1.1-alpha.4

The three Linux x86-64 source packages are published and were installed directly
from their public registries on a clean Ubuntu 24.04 droplet. The packages build
Skarve's native core locally; they are not prebuilt binaries or a claim of
Windows/macOS support.

| Interface | Public package | Registry-install result on Ubuntu 24.04 |
|---|---|---|
| Python and `skarve` CLI | [PyPI `skarve-engine==0.1.1a4`](https://pypi.org/project/skarve-engine/0.1.1a4/) | Fresh Python 3.12 venv built the source wheel, installed it, reported the expected CLI version and returned the native backend descriptor. |
| Node.js | [npm `@skarve/engine@0.1.1-alpha.4`](https://www.npmjs.com/package/@skarve/engine/v/0.1.1-alpha.4) | Fresh npm 10/Node 20.20.2 install built the native core; default and named imports resolved and a `Skarve` session opened and closed. |
| Rust and `raster-engine` CLI | [crates.io `skarve@0.1.1-alpha.4`](https://crates.io/crates/skarve/0.1.1-alpha.4) | `cargo install skarve --version 0.1.1-alpha.4 --locked` built the CLI; its version and native backend descriptor passed. |

The disposable DigitalOcean host was Ubuntu 24.04.4 x86-64, 2 vCPU, 4 GB,
GDAL 3.8.4, libdeflate 1.19, Rust 1.98.1, and Node 20.20.2. It used the
$0.036/hour plan without backups or extra volumes. Build work was serialized
to stay within its memory. The npm install took about 3 minutes; Cargo took
3 minutes 34 seconds; Python's source wheel took several minutes. Archive
downloads were small relative to compilation. These times are observations
on one shared VM, not service guarantees.

The packages came from the same tested source commit
`d0447ee7f5fd1dba54601c1e23e02de601d8257f`, tagged
`v0.1.1-alpha.4`. The npm registry integrity matched the tested tarball;
the crates.io checksum matched the tested `.crate` archive. PyPI's published
source archive SHA-256 is
`8c7bd74fb118b05554428fb7daaa465670037c0a20c0db7bc9591ce208002ec8`,
which differs at the archive-byte level from the prior candidate. A member-by-member
comparison found all 853 paths and file contents identical. The PyPI workflow
[completed successfully](https://github.com/FlorentCf/skarve/actions/runs/35928770859)
from public `main` after the one-line checksum-path fix in PR #6.

The earlier [Debian 13 clean-host archive test](PACKAGE_SOURCE_DEBIAN13_2026-09-23.md)
exercised synthetic TIFF and SKV numerics: Python and Node matched the
independent fractional-sum oracle (`1845.125`), direct and summary-indexed
results, complete paged batches, and SKV logical digest; the Rust CLI matched
the TIFF result. The new Ubuntu registry test verified installation, native
loading, version, backend availability, and Node session lifecycle. It did
not repeat the full numerical suite or test Windows, macOS, other CPU
architectures, optional exactextract, or a Horizon Mapper deployment.

Cleanup: DigitalOcean droplet `603145007` was destroyed after the checks;
the account confirmed deletion of the temporary SSH key, and its local private
key, public key, and known-host file were removed. The temporary npm CLI
session was logged out; the one-time crates.io publishing token was revoked.

## What to improve next

- Keep the published package versions immutable. Update documentation in a
  separate change when registry availability or platform support changes.
- Source builds need Rust, a C toolchain, GDAL and libdeflate development
  packages. A true one-command install on a bare host would need reviewed
  per-platform binaries, dependency/licensing decisions, and a qualification
  matrix. Do not imply that this alpha already provides one.
- Python's installed CLI is `skarve`; Cargo's remains `raster-engine` for
  compatibility. A future alias could make the Rust command easier to find.
- The npm archive's embedded README was written before registry publication
  and still describes an older local candidate. The repository documentation
  has been corrected here; the immutable published alpha cannot be edited.