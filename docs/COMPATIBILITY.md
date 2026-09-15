# Version and compatibility policy

`0.1.1-alpha.1` is an isolated experimental pre-release; `0.1.0-beta.2` is preserved
separately. Pin the complete version and artifact SHA-256 in
deployments. Do not combine a binding from one artifact set with a native library
from another. Each package embeds its source and native identities, policy list
and bulk ABI version. Existing `raster_engine_lab` Python and `RasterEngine` Node
aliases remain compatibility names; the public product is Skarve.

The selected-buffer C ABI is version 1. Unsupported versions, modes, reducers,
strides, masks and resource requests fail explicitly. Changes to a policy's
numerical meaning require a different policy identifier; do not infer meaning
from the host application's name. Generic source-owned coverage does not imply
center selection, overview choice or domain bucketing semantics.

Prepared indexes are optional acceleration artifacts, not portable substitutes
for source provenance. Reopen only with the matching interpretation, original
source and supported index format/build identity. A source update invalidates
the index; rebuilding charges source reads and storage again. Current local stat
identity is a mutation detector, not a cryptographic content guarantee. Trusted
content identity must be supplied explicitly where supported. Never silently
bind an old index to a changed source or reinterpret an old format.

Before a stable release, incompatible API/index changes may occur with a new
pre-release identifier and changelog entry. Released artifacts are immutable;
corrections use a new version. No cross-version index compatibility or platform
support is promised without a recorded test.

SKV v0 is unstable and separate from the existing summary-only index format.
It carries its own raw samples and serving generation, so the original source
is unnecessary to reopen it. Readers reject unknown required feature bits;
the optional `byte_delta_v1` predictor uses an explicit feature bit, while old
v0 files without that field remain readable. There is no promise of future SKV
format compatibility without a new recorded qualification. Update by compiling
a new immutable file and charging its full source/build/storage cost.
