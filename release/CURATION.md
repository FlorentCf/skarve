# Curated source and publication boundary

The independent Git history begins at f1fc603. It imports the product source at d15b0bccdf87654e6e6bd221ce114fb4f129e42d without importing the research repository's Git objects. BASELINE_EXPORT.json records every imported path, source Git blob and SHA-256. No source rasters, HM application code, credentials, native binaries or private trace archive are part of this product history.

The launch changes add a safe Rust facade, idiomatic examples and external-crate qualification; update version and packaging/CI documentation; preserve the supplied SVG files; and export the final benchmark into inspectable, bounded public tables/charts. Within src/, only api.rs and the lib.rs facade exports differ from d15. Mathematical kernels, source readers, SKV reader/writer and optional native exactextract bridge remain unchanged.

The small historical beta reports and their losses remain as contextual evidence. The product contains the current benchmark worker and synthetic preparation/reproduction scripts, not the research branches or speculative prototypes. Historical absolute source references are replaced with stable non-downloadable logical identifiers in the public evidence. Numerical observations are unchanged and verified by benchmarks/native-skv/verify.py. The original private evidence and frozen artifacts remain untouched.

Publication audit scope: all files and reachable commits/refs in this independent history, nested source/package archives and dependency notices. The audit reports matching locations without revealing matched secrets. Its targeted checks do not prove the absence of arbitrary vulnerabilities or resolve legal rights. No public repository exists at the proposed URL yet; Cargo publication and npm registry publication remain disabled.

Authors and third-party notices are retained. Apache-2.0 is an owner-confirmation proposal for original source. Branding rights and distribution of binaries with the system dependency closure require separate decisions; see DISTRIBUTION.md. Source-only publication is the proposed initial scope.

Inherited generated SVG/CSV whitespace and three pre-existing test-file blank endings were preserved so historical bytes did not change merely to silence whitespace checks. Newly authored code and documentation receive focused whitespace/format checks.
