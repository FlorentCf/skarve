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

Current-build commands, results, artifact hashes and limitations will be recorded in the qualification receipt before review is marked complete. No historical pass count is substituted for this candidate's test run.
