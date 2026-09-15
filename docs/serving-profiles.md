# Verified serving profiles

An experimental serving profile lets the engine choose between two equivalent
representations for an ordered query. The consumer supplies the actual selection,
the required logical view and the numerical policy. Skarve checks the profile,
selects one source, opens it, executes the existing ordered kernel and closes it.
The unused representation receives no metadata or pixel requests.

This interface currently supports `hm_demographics_ordered_v1` over Float32 or
Float64 sources. It is intended for an application that has already established
its source year/tier, precision, exact center mask and logical window order.
It does not choose an overview, resample a source, construct a polygon mask or
change the requested numerical policy. Ordinary `infuse`, `carve` and `cleave`
remain available for direct TIFF/COG, SKV and other supported source contracts.

## Consuming a profile

```python
import json
from skarve import Skarve

profile = json.load(open("serving-profile.json"))
selection = json.load(open("selections.json"))
with Skarve() as sk:
    answer = sk.sum_selected(profile, selection,
        numerical_policy="hm_demographics_ordered_v1",
        view_id="the-approved-logical-view",
        access_class="the-qualified-access-environment")
print(answer["rows"], answer["routing"])
```

Node uses `await sk.sumSelected(profile, selection, {numerical_policy, view_id,
access_class, signal})`. Python also provides `await sk.sum_selected_async(...)`
with the normal cooperative cancellation and drain contract. Both call the
native session directly; no Python worker is required for Node or the CLI.

```sh
skarve sum-selected --profile serving-profile.json selections.json \
  --view-id the-approved-logical-view \
  --numerical-policy hm_demographics_ordered_v1 \
  --access-class the-qualified-access-environment
```

The installed `python_serving.py` and `node_serving.mjs` examples consume these
same files. Access classes are operator-declared performance conditions, not
runtime network measurements. Omission or `unknown` selects the paired direct
source. A deployment should pin one measured profile and its access class;
consumers do not select a file format on each query.

## Selection and failure contract

`accelerated_when` is one bounded, conjunctive rule. It checks the actual selected
band set, polygon and logical-window counts, selected cell count, and the width,
height and area of the union of nonempty logical windows. The envelope describes
query inputs, not a promised decoder footprint. Empty band selection means all
exposed bands. Permutations preserve the supplied output order. The rule must
be backed by measurements of the complete operation for that exact source view
and access environment; a large band count alone is not sufficient evidence.

When the performance rule does not match, Skarve uses the paired direct source.
A wrong view, unsupported policy, explicit NoData override, malformed selection,
invalid attestation or changed identity is an error. An open, verification or
execution failure never triggers a hidden retry with the other representation.
There is no persistent preselected handle, so later requests cannot accidentally
reuse an earlier request's eligibility decision.

Results retain the ordinary ordered sums, validity counters and provenance.
`routing` adds the selected role, reason, actual request facts, content pin,
verification authority and phase timings. It omits source locations, headers
and credentials. `complete_profile_ms` includes routing, opening, execution and
source closure; complete consumer benchmarks must additionally include native
response serialization, binding transfer and consumption.

## Provisioning and trust

The profile is a trusted owner artifact, not an untrusted end-user instruction.
It contains `direct` and `accelerated` ordinary `SourceSpec` values, each with an
explicit SHA256/byte-length identity and its exact expected exposed view. These
roles do not require particular source formats. An `owner_verified_full_view_v1`
attestation binds the two physical pins, view selectors, expected views, complete
typed-sample and mask digests, full-interpretation digest, verifier version and
retained verification-receipt hash.

Two file hashes do **not** establish equivalent pixels. Neither does metadata
equality. The verifier must compare the entire admitted view's typed sample bits,
mask bytes, grid, band order, NoData, scale/offset, units, descriptions and pixel
convention before issuing the attestation. Source-specific provenance may differ
only where explicitly represented, such as an original band index or the stored
overview from which a self-contained SKV was compiled. A cropped view cannot
silently fall back to a whole-country raster with different coordinates.

Skarve validates the attestation's structure, bindings and selected source's
exposed interpretation at execution. It trusts the owner's equivalence claim;
it does not reopen both sources to re-prove their equality. With identity policy
`verify`, the selected object's full content hash is checked during opening and
belongs inside cold timing. `trusted_manifest` instead relies on the documented
immutable-object authority and generation/ETag checks; it must not be described
as a fresh full-content verification.

For provisioning, Python `source.inspect()`, Node `await source.inspect()`, or native
`source_info`, includes `serving_profile_view`. All floating metadata is encoded
as fixed 16-character lowercase IEEE754 bit strings, preserving signed zero and
avoiding JavaScript integer rounding. The profile's SHA256 fields use compact
UTF8 JSON with recursively sorted object keys and array order preserved. The
source selector contains normalized `format`, `variable`, `overview`, `crs`,
`longitude_shift` and `bands`, including null/default values. Source locations
and credentials are not part of the view selector.

The source distribution includes `tests/skv_format_oracle.py` for independent
format/source comparison and `tests/serving_profile_fixture.py` as a generated
provisioning example. The latter's small fixture rule is for contract tests and
is not a performance recommendation. Production profiles need their own pinned
source evidence and measured rule.

## Bounds and scope

Profiles are limited to64KiB, two candidates and one rule. Native control, source
opening/retention and ordered execution reservations are checked before work;
the ordinary HTTP, planning, contribution and materialized-read limits remain
in force. No raw raster values cross the JSON interface. Cancellation is checked
through routing and execution, and source ownership ends before success returns.

This is a narrowly scoped dispatch layer over existing readers and mathematics.
It is not a new raster format requirement, a general query optimizer or proof
that one representation is universally faster. The benchmark report supplies
the tested serving configuration, exclusions and retained losses.
