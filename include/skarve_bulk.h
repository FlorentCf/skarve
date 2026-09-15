#ifndef SKARVE_BULK_H
#define SKARVE_BULK_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* ABI v1: fixed layout on 64-bit little/big-endian platforms using native endian
 * scalars. This header intentionally does not change the resident raster API.
 * Every pointer must refer to a correctly sized live allocation, immutable for
 * the entire synchronous call. Native retains no pointer. Raw C callers own
 * that allocation precondition; scalar bounds checks cannot validate a lying
 * address. No concurrent re_drop. re_cancel is allowed.
 */
#define SKARVE_BULK_ABI 1
#define SKARVE_BULK_MAX_BANDS 64
#define SKARVE_BULK_MAX_WINDOWS 4096
#define SKARVE_BULK_MAX_BYTES UINT64_C(134217728)
#define SKARVE_BULK_MAX_CONTRIBUTIONS UINT64_C(268435456)
enum { SKARVE_STRICT_SELECTED_V1=1, SKARVE_HM_DEMOGRAPHICS_ORDERED_V1=2 };
enum { SKARVE_F32=1, SKARVE_F64=2 };
enum { SKARVE_VALID_ALL=0, SKARVE_VALID_BYTES=1, SKARVE_VALID_BITS_LSB=2 };
enum { SKARVE_SELECT_ALL=0, SKARVE_SELECT_U32=1, SKARVE_SELECT_U64=2,
       SKARVE_SELECT_SPANS=3 };
enum { SKARVE_SUM=1, SKARVE_MIN=2, SKARVE_MAX=4, SKARVE_MEAN=8 };
enum { SKARVE_HAS_NODATA=1 };
enum { SKARVE_RESULT_HAS_VALUES=1 };
enum { SKARVE_BULK_OK=0, SKARVE_BULK_INVALID=1, SKARVE_BULK_BUSY=2,
       SKARVE_BULK_CANCELLED=3, SKARVE_BULK_NONFINITE=4, SKARVE_BULK_PANIC=5 };

typedef struct {
    const void *data;
    uint64_t data_bytes, byte_offset, byte_stride, cell_count;
    const uint8_t *validity;
    uint64_t validity_bytes, validity_offset;
    double nodata;
    uint32_t band_id, dtype, validity_kind, flags;
} SkarveBulkBand; /* 88 bytes */
typedef struct { uint64_t start, count; } SkarveBulkSpan;
typedef struct {
    const void *data;
    uint64_t data_bytes, count;
    uint32_t kind, flags;
} SkarveBulkSelection; /* 32 bytes */
typedef struct {
    const SkarveBulkBand *bands;
    uint32_t band_count, flags;
    SkarveBulkSelection selection;
} SkarveBulkWindow; /* 48 bytes */
typedef struct {
    uint32_t abi_version, struct_size, policy, reducers;
    const SkarveBulkWindow *windows;
    uint64_t window_count, max_payload_bytes, max_contributions, flags;
} SkarveBulkRequest; /* 56 bytes */
typedef struct {
    uint32_t band_id, flags;
    double sum, min, max, mean;
    uint64_t valid_count, excluded_mask, excluded_nodata;
    uint64_t excluded_nonfinite, excluded_negative;
} SkarveBulkResult; /* 80 bytes */
typedef struct {
    uint32_t abi_version, struct_size;
    uint64_t window_count, band_count, payload_bytes, selection_bytes;
    uint64_t result_bytes, native_owned_bytes, validation_ns, reduction_ns;
} SkarveBulkMetadata; /* 72 bytes */

/* max_* = 0 means the fixed default cap above, never unlimited. A caller may
 * reduce but not increase the caps. policy=0 and reducers=0 select strict and
 * sum. Validity offset is in bytes for BYTES, bits for BITS_LSB. ALL requires
 * null/zero validity fields. Selection ALL requires null/zero data/count.
 * Indices/spans form an ORDERED MULTISET, not a set. Each window has identical
 * ordered unique band IDs and a common cell_count across bands. Nodata is
 * exact equality (including NaN-to-NaN exclusion); classification precedence
 * is mask, nodata, nonfinite, negative (negative only excluded by HM).
 * Counts count selected contributions, including repeated indices.
 *
 * HM uses binary64 left-fold from +0 per band/window, then window left-fold
 * from +0 in supplied order. Native never applies overview scaling or display
 * rounding. Strict uses the existing compensated Sum across contributions.
 * Only requested extrema/mean are computed. Unrequested fields are +0.
 * Empty sum is +0 and HAS_VALUES is clear; empty extrema/mean are +0 placeholders.
 *
 * Payload bytes conservatively sum descriptor data_bytes and validity_bytes
 * plus selection data_bytes, even if caller allocations alias. Selection bytes
 * are included in payload bytes, not additional. Native owned bytes accounts
 * bounded accumulator/result workspace, not borrowed metadata or caller pages.
 *
 * Result capacity is in records; the error capacity is in bytes including NUL
 * (0 allows null). On failure, results AND metadata remain untouched. Error
 * text is bounded and NUL-terminated when capacity>0. Success clears error[0].
 * Metadata/result must not alias input or each other. No partially valid result.
 */
struct Handle;
int32_t re_bulk(struct Handle *, const SkarveBulkRequest *,
                SkarveBulkResult *, uint64_t result_capacity,
                SkarveBulkMetadata *, uint8_t *error, uint64_t error_capacity);
#ifdef __cplusplus
}
#endif
#endif

