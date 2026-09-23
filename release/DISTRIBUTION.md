# Distribution decision and owner gates

**Current candidate: source-built 0.1.1-alpha.4 registry packages.** Apache-2.0 licensing and publication of the separate public source repository were approved for alpha.1. The owner has now requested public Python, npm and Rust installation with fresh-host testing. The prior alpha.3 source release and local binary candidates remain immutable. Native binary archives remain outside this publication scope. The d15 benchmark remains identified as d15, not a timing claim about rebuilt artifacts.

| Deliverable | Technical disposition | Owner gate |
|---|---|---|
| Curated Skarve Rust/CLI/Python/Node source and SKV v0 specification | Public Apache-2.0 source baseline; alpha.3 adds bounded large grouped SKV capacity with synthetic correctness tests | Public source baseline already established |
| crates.io | Source-only crate eligible for publication; external consumer and clean install must pass | Registry ownership and authentication required; no upload yet |
| npm/PyPI | Source-only packages compile the included native Rust source on the consumer host | Registry scope/name ownership, authentication and fresh-host tests required; no upload yet |
| Native Linux wheels/tarballs or container | Exact dynamic runtime inventory and notices retained; no container bundled | Unresolved combined-distribution obligations require appropriate review before public redistribution |

## Approved source license

The public source uses the existing unmodified Apache-2.0 text for Florent Chif's owned code, as approved for alpha.1. It permits commercial reuse and competing implementations. Redistribution requires the applicable license, notices and modification information; it does not guarantee a visible promotional credit. Software licensing does not grant Skarve branding rights. Upstream components retain their own terms. [Apache license](https://www.apache.org/licenses/LICENSE-2.0).

## Binary dependency checklist

The source license is not a declaration that every linked binary is Apache-only. The qualified Ubuntu GDAL build links additional libraries, including GPL/LGPL families. Dynamic loading and separately installing them do not automatically settle combined-work obligations. [GDAL's licensing guidance](https://gdal.org/en/stable/license.html) and [Apache's GPL compatibility discussion](https://www.apache.org/licenses/GPL-compatibility.html) support keeping this separate gate.

- [x] Identify GDAL, libdeflate and optional exactextract/GEOS direct relationships.
- [x] Preserve pinned Cargo/Node/C++ inventory, original notices and exact Ubuntu package/source versions.
- [x] Keep the frozen binary hashes and query evidence independent of any rebuild.
- [x] Choose source-first publication scope while the binary gate is unresolved.
- [ ] Obtain a determination for the specific combined binary/dependency closure, including applicable GPL exceptions and LGPL conditions.
- [ ] If distributing covered object code, prepare and verify the appropriate corresponding source/relinking/notice obligations for that distribution method; a generic upstream link is insufficient.
- [ ] Alternatively qualify a different compliant dependency closure under a new artifact identity and repeat relevant installation/correctness tests; this pass does not undertake a GDAL rebuild.
- [ ] Approve a specific binary release manifest only after these obligations are closed.

The complete inventory is in [RUNTIME_LICENSES.md](../docs/RUNTIME_LICENSES.md), [SYSTEM_RUNTIME.json](../third_party/SYSTEM_RUNTIME.json) and [THIRD_PARTY.md](../THIRD_PARTY.md). This technical inventory is not legal clearance. Building a source archive or completing a local test does not itself publish a registry version, binary, container, production deployment or announcement.
