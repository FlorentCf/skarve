# Source-only alpha 2 review

Candidate: `0.1.1-alpha.2` (Python `0.1.1a2`). This is a review candidate, not a published release. Registry publication remains disabled. The existing alpha 1 release and its evidence are unchanged.

## Scope and provenance

The candidate starts from public `9d4bb943` and includes the complete public portability PR #1 (`644951dd`). Reusable engine changes were independently reviewed and transferred by an explicit file allowlist rather than merging the development repository history. Original authorship and third-party notices are retained.

Included: original typed windows and masks through Rust/C/Python/Node; position-aware buffer admission and grouped reads; retained-source query budgets and final verification; shared HTTP pools; bounded two-request preparation; opt-in TIFF metadata prefixes/pages and accounted cache capacity; a 131,072-leaf SKV capacity limit; and compiler admission for every actual source window. The SKV wire format and numerical reduction policies are unchanged.

The separate compiler correction was reconciled with the later transport changes, rather than replacing them with its older branch. Python now admits the same bounded grouped window band count as Node. Generated tests cover the new interfaces; transport tests exercise pooling enabled/disabled and concurrency one/two.

Excluded: application code, deployment settings, credentials, serving manifests, private object locations, real-data fixtures, private benchmark receipts and development artifact archives. No new application-specific numerical policy is introduced. Existing public compatibility policies are retained unchanged.

## Release boundaries

- Source only. Local wheels, npm archives, CLI archives and the Rust crate are built for installation tests, not cleared for public binary redistribution.
- Apache-2.0 remains the approved source license; upstream terms and notices remain applicable.
- There is no new performance claim. Historical benchmarks identify their original runtime; this candidate's checks establish integration and correctness.
- HTTP cancellation is cooperative and blocking requests can last until their timeout. Sources require exclusive ownership during operations. Connection-pool registry size is not a process RSS or socket cap.
- Finalized verified queries are required before exposing provisional window data. See [source windows](../docs/source-windows.md).
- Review [distribution gates](DISTRIBUTION.md) before any later publication.

## Validation

[The qualification receipt](ALPHA2_QUALIFICATION.json) records exact tested source revisions, runtime source hashes and artifact identities. Both builds have identical engine source hashes. Later documentation and example-version corrections do not change the engine implementation.

| Check | Native | Optional exactextract |
|---|---:|---:|
| Rust tests and doctests | 316 passed | 329 passed |
| Python public suite | 196 passed | 196 passed |
| Node public suite | 17 passed | 17 passed |
| Fresh installed CLI/Python/Node checks | 58 passed | 64 passed |
| Backend numerical/eligibility contracts | 35 passed | 60 passed |
| HTTP pool/concurrency configurations | 4 passed | 4 passed |
| Generated typed-window/metadata/admission suite | passed | passed |
| Benchmark smoke | passed | passed |

One Cargo HTTP test is intentionally delegated to its controlled Python server harness. External Rust crate extraction/compilation, strict TypeScript consumer checking, and both C headers also passed. The source archive was verified and repackaged without Git; its regenerated source archive was byte-identical. Native and exactextract GitHub CI plus dependency review passed on the recorded revision; the PR checks show the latest documentation revision's recheck.

The audit found and corrected Python's narrower grouped-band admission, overflow for an oversized cache request, a sequential-order assumption in a parallel transport test, missing installed-window checks, and stale example version constraints. The first local optional build correctly rejected missing prerequisites; locally extracted matching GEOS headers and CMake resolved that environment issue without replacing GDAL/GEOS. No numerical safeguard was relaxed.

### Reproduction

Use the exact revision from the receipt with the declared prerequisites. The complete authoritative two-backend sequence is in [CI](../.github/workflows/ci.yml). Its principal commands are:

```sh
python scripts/build.py --test
python scripts/test-source-windows.py --library target/release/libraster_engine.so --work scratch/windows
python scripts/test-rust-package.py --work scratch/rust-package --target-dir target/rust-package
python -m pytest -q tests
node --test bindings/node/*.test.mjs bindings/node/test.mjs tests/product_api.test.mjs
python scripts/package_release.py --local-use-only --output-dir dist/native
python tests/standalone_suite.py --artifacts dist/native --wheelhouse scratch/wheelhouse --output scratch/installed.json
```

CI specifies wheelhouse creation, dependency installation, the 2×2 transport matrix, smoke commands and optional-backend repetition with `--exactextract`. Paths used for generated outputs must be new. The receipt's private local binary archives are qualification artifacts; the proposed release remains source-only.

### Recommendation

Review this source-only alpha and its included portability fix together. Keep #1 open until the owner chooses whether to merge it first or let this candidate supersede it. No release tag, registry upload, public binary distribution, main merge or application deployment has been performed. There is no remaining engineering blocker in the tested scope; publication and binary-distribution decisions remain separate owner actions.
