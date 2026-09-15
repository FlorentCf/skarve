# Distribution decision and owner gates

**Prepared release scope: source-first alpha. Binary packages remain private for qualification.** Existing alpha0 packages are immutable. New consumer/docs work uses 0.1.1-alpha.1 and new hashes; the d15 benchmark remains identified as d15, not a timing claim about rebuilt artifacts.

| Deliverable | Technical disposition | Owner gate |
|---|---|---|
| Curated Skarve Rust/CLI/Python/Node source and SKV v0 specification | Independently staged source tree, with notices, provenance, synthetic tests and sanitized benchmark evidence | Confirm Apache-2.0 for owned source and permission to create/publish the separate Skarve repository |
| crates.io | Rust external-consumer/package checks; `publish = false` retained | Separate name/ownership and explicit registry publication approval; no upload attempted |
| npm/PyPI | Installable private artifacts; npm remains private | Separate package publication and binary-distribution approval |
| Native Linux wheels/tarballs or container | Exact dynamic runtime inventory and notices retained; no container bundled | Unresolved combined-distribution obligations require appropriate review before public redistribution |
| HM adapter/page | Separate review branches, initial legacy mode | Review/merge and staging/cloud permissions separately; production activation and announcement are separate actions |

## Source license proposal

Retain the existing unmodified Apache-2.0 text for Florent Chif's owned code, subject to owner confirmation. It permits commercial reuse and competing implementations. Redistribution requires the applicable license, notices and modification information; it does not guarantee a visible promotional credit. Software licensing does not grant Skarve branding rights. Upstream components retain their own terms. [Apache license](https://www.apache.org/licenses/LICENSE-2.0).

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

The complete inventory is in [RUNTIME_LICENSES.md](../docs/RUNTIME_LICENSES.md), [SYSTEM_RUNTIME.json](../third_party/SYSTEM_RUNTIME.json) and [THIRD_PARTY.md](../THIRD_PARTY.md). This technical inventory is not legal clearance. No public binary, registry release, container, production deployment or announcement is authorized by preparing it.
