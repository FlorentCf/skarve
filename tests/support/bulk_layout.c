#include <stddef.h>
#include "skarve_bulk.h"
_Static_assert(sizeof(SkarveBulkBand) == 88, "band layout");
_Static_assert(sizeof(SkarveBulkSelection) == 32, "selection layout");
_Static_assert(sizeof(SkarveBulkWindow) == 48, "window layout");
_Static_assert(sizeof(SkarveBulkRequest) == 56, "request layout");
_Static_assert(sizeof(SkarveBulkResult) == 80, "result layout");
_Static_assert(sizeof(SkarveBulkMetadata) == 72, "metadata layout");
_Static_assert(offsetof(SkarveBulkBand, validity) == 40, "validity offset");
_Static_assert(offsetof(SkarveBulkBand, nodata) == 64, "nodata offset");
_Static_assert(offsetof(SkarveBulkBand, band_id) == 72, "id offset");
_Static_assert(offsetof(SkarveBulkRequest, windows) == 16, "windows offset");
_Static_assert(offsetof(SkarveBulkResult, valid_count) == 40, "count offset");
_Static_assert(offsetof(SkarveBulkMetadata, validation_ns) == 56, "timing offset");

