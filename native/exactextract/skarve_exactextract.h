#ifndef SKARVE_EXACTEXTRACT_H
#define SKARVE_EXACTEXTRACT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SKARVE_EE_ABI_VERSION 2
#define SKARVE_EE_STAT_COUNT 5
#if defined(__GNUC__)
#define SKARVE_EE_API __attribute__((visibility("default")))
#else
#define SKARVE_EE_API
#endif

typedef struct {
    double xmin, ymin, xmax, ymax, dx, dy;
    uint64_t width, height;
} SkarveEeGrid;

typedef struct {
    const uint8_t *data;
    size_t length;
} SkarveEeWkb;

/* Callback-owned, contiguous row-major, normalized binary64. The bridge borrows
 * these buffers until release(ctx, lease), exactly once after a successful read.
 * An absent validity pointer means all values valid. A successful read must
 * return length == width*height, a non-null lease, and valid finite values.
 * No callback may throw or unwind. On a failed read the callback owns cleanup.
 * Callbacks must not reenter the same session or any exactextract operation. */
typedef struct {
    const double *values;
    const uint8_t *valid;
    size_t length;
    void *lease;
} SkarveEeWindow;

typedef int32_t (*SkarveEeRead)(void *ctx, size_t source,
    uint64_t x0, uint64_t y0, uint64_t width, uint64_t height,
    SkarveEeWindow *window, char *error, size_t error_capacity);
typedef void (*SkarveEeRelease)(void *ctx, void *lease);
typedef int32_t (*SkarveEeCancelled)(void *ctx);

typedef struct {
    uint32_t abi_version;
    uint32_t strategy; /* 0 feature-sequential; 1 raster-sequential. */
    const SkarveEeGrid *sources;
    size_t source_count;
    const SkarveEeWkb *features;
    size_t feature_count;
    uint64_t max_cells;
    uint64_t max_live_window_bytes;
    void *context;
    SkarveEeRead read;
    SkarveEeRelease release;
    SkarveEeCancelled cancelled;
    uint32_t statistics_mask; /* Bits 0..4: sum/support/mean/min/max; nonzero. */
    uint32_t reserved; /* Must be zero. Support is always computed internally. */
} SkarveEeRequest;

typedef struct {
    uint64_t read_calls;
    uint64_t read_cells;
    uint64_t read_bytes;
    uint64_t peak_live_window_bytes;
    uint64_t callback_nanoseconds;
    uint64_t upstream_nanoseconds; /* Includes nested callbacks and sink writes. */
    uint64_t total_nanoseconds;
} SkarveEeMetrics;

/* Blocking synchronous call; caller retains request/buffer ownership until
 * return. No hard process-RSS bound or hard cancellation guarantee is supplied.
 * Cancellation is cooperative at queue/read/feature/progress/output boundaries.
 * Upstream geometry/reduction operations themselves are not interruptible.
 * Output layout [feature, source, statistic], statistic order sum, support,
 * mean, min, max. Undefined entries have defined=0 and a zero placeholder;
 * support is fractional, never an integer cell count. Unrequested slots have
 * defined=0 and zero placeholders, except support, computed internally for
 * validity/status even if not requested. Output is caller-staged:
 * on ANY nonzero return all output must be discarded (partially written data
 * is possible). C++ allocations are released before return. Status: 0 success,
 * 1 contract/internal/upstream failure, 2 cooperative cancellation, 3 callback
 * failure, 4 resource admission/budget failure. Error is NUL-terminated when
 * capacity > 0. All calls serialize because pinned upstream MapFeature uses a
 * shared GEOS context. Reentrant invocation fails instead of deadlocking. */
SKARVE_EE_API int32_t skarve_ee_execute_v2(const SkarveEeRequest *request,
    double *values, uint8_t *defined, size_t output_length,
    SkarveEeMetrics *metrics, char *error, size_t error_capacity);

SKARVE_EE_API const char *skarve_ee_upstream_version(void);
SKARVE_EE_API const char *skarve_ee_upstream_commit(void);
SKARVE_EE_API const char *skarve_ee_geos_version(void);

#ifdef __cplusplus
}
#endif
#endif
