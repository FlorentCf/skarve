# Runtime licensing boundaries

Skarve's own proposed project license and the terms applying to a particular
linked runtime are separate questions. This document records a finite review
of the Ubuntu 24.04 x86-64 development configuration. It does not certify that
every resulting binary combination can be distributed under Apache-2.0 alone.

The inspected experimental library directly links `libdeflate.so.0` for SKV
decoding and `libgdal.so.34`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6` and the
Linux dynamic loader. The exactextract-enabled build also directly links
`libgeos_c.so.1` and `libstdc++.so.6`. On the inspected host, these dependencies
and their transitive dependencies resolve to 107 libraries from 94 Ubuntu
packages. Library bytes are not bundled. Exact package/source versions, the
inspected native hash, direct versus transitive relationships and copyright
records are in [SYSTEM_RUNTIME.json](../third_party/SYSTEM_RUNTIME.json).

The qualified decoder runtime is Ubuntu `libdeflate0:amd64` version
`1.19-1build1.1`. Its existing distribution copyright record remains in the
inventory; the change makes its direct use by Skarve explicit. The independently
authored Rust wrapper calls the upstream library and does not copy its decoder.
Each assembled artifact records its own inspected binary hash and actual ELF
dependencies, including when exactextract is absent. The Linux loader requires
the libdeflate runtime and the per-context API symbols; there is no silent
Linux decoder fallback when those are missing.

Some dependencies support GDAL formats that the ordinary Skarve workflow does
not use, but they are still dependencies of this system GDAL build. In
particular, the GDAL ELF directly names Poppler (PDF), libheif (HEIF), GEOS,
MySQL and other optional format/feature dependencies. Disabling a feature in a
query does not remove those ELF relationships.

| Observed component | Distribution copyright review | Interpretation |
|---|---|---|
| GDAL | Main source broadly Expat/MIT; some generated parser code has a Bison exception and other files retain their own terms | The upstream GDAL project warns that dependencies may make a binary's overall terms less permissive than MIT. |
| Poppler | Default source stanza declares GPL-2 or GPL-3 | A concrete GPL-family dependency, not just a build-script label. |
| GEOS | Default source LGPL-2.1+ | Applicable library-use and redistribution conditions need to be respected. |
| libheif | Default source LGPL-3+; separate scripts/packaging include GPL labels | Do not misclassify the library by taking a union of every package copyright label. |
| librttopo and JBIG-KIT | Default source GPL-2+ | Additional transitive GPL-family components. |
| MySQL client | GPL-2 material with an explicitly recorded Universal FOSS Exception | Read the actual exception; neither ignore it nor treat every GPL label as an unconditional restriction on all linked code. |

The inventory's complete `license_labels` includes test, documentation, build
and Debian packaging stanzas. That union is deliberately not a binary-license
classification. Preserved full copyright files identify which terms apply to
which upstream files, and include the MySQL exception. Common GPL/LGPL texts
referenced by those records are retained under `third_party/notices/system/common`.

Before binary redistribution, establish which license conditions apply to the
specific linked combination and how they will be satisfied. If GPL-covered
object code is distributed, source provision and license/notice conditions
depend on the applicable GPL version and distribution method; a package name
or a link to a home page is not itself a corresponding-source offer. LGPL
conditions likewise include applicable notices and the user's ability to use
modified library versions or relink, depending on version and mechanism.
The ordinary system dynamic-link mechanism and an unmodified library are facts
to consider, not an automatic legal clearance.

Apache's compatibility guidance distinguishes GPLv3 from GPLv2 and warns against
assuming that a GPL-dependent combination can simply be labeled Apache-only.
That guidance does not decide whether this particular indirect runtime
relationship creates a derivative or combined work, or which exceptions apply.
The current review leaves that distribution decision to the rights holder with
appropriate legal advice. Source ownership/license approval does not silently
approve binary-distribution terms.
[Apache compatibility guidance](https://www.apache.org/licenses/GPL-compatibility.html),
[GDAL licensing guidance](https://gdal.org/en/stable/license.html).

No complete corresponding-source bundle for the Ubuntu runtime has been
prepared or offered here. The inventory records source package/version pairs
so they can be obtained from the matching Ubuntu source repositories if
needed, using `apt-get source SOURCE_PACKAGE=SOURCE_VERSION` after configuring
the appropriate `deb-src` entries. Source availability and any actual source
offer must be verified for the chosen distribution; no download or system
configuration is performed by this document.
