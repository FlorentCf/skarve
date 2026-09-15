# Cropped NumPy reference validity

The first development programme retained six unsupported native numeric/five
requests and five failing independent array-reference groups. None was accepted
as a benchmark pass. Native compact output requires count as a sixth statistic;
paired full-output controls request exactly the same five statistics.

The pinned exactextract 0.3.0 Python wheel also produced different answers for
a cropped masked NumPy source between its two strategies. The reproducible
8×10 fixture has one invalid cell whose stored payload is −9999. Feature
traversal returns sum 1032.3125, support 25.125 and minimum 12. Raster traversal
includes that payload, returning −8966.6875, 26.125 and −9999. These are observed
reference results, not an asserted diagnosis of every internal cause.

The [pinned NumPy source implementation](https://github.com/isciences/exactextract/blob/v0.3.0/python/src/exactextract/raster.py)
returns array windows; the retained probe records the actual installed module
and extension hashes. The compatible reference representation copies the
already normalized array and changes only invalid entries to explicit NaN
nodata. Both strategies then match the independent exact-Fraction answer, and
every valid binary64 bit is asserted unchanged. The cropped-mask regression
also checks that the input array itself remains unchanged.

```sh
python benchmarks/beta2/reference_mask_probe.py --output scratch/reference-validity.json
python -m pytest -q tests/exactextract_harness.py
```

Natural original-file controls remain primary for the identity-scale timing
fixtures and are unchanged. Array decoding, invalid-only copy and oracle setup
are charged in the separate unscored reference timer. This correction does not
improve measured engine timings, widen a tolerance, weaken the reader mask
contract, or establish equivalence from the earlier 26 small calibration rows.
