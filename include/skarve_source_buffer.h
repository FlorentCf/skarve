/* Original scalar window output ABI v1. Apache-2.0; see docs/source-windows.md. */
#ifndef SKARVE_SOURCE_BUFFER_H
#define SKARVE_SOURCE_BUFFER_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* session is a live re_new handle. control is immutable UTF-8 JSON, max8KiB.
   output is exclusively writable, capacity<=64MiB, alive through completion.
   Never concurrently drop session. JSON contains descriptors, never pixels.
   Use output only when returned envelope has ok:true. On failure discard output.
   re_cancel is cooperative; wait for completion before release/drop/reuse.
   Free metadata via re_free_string (from the core ABI).
*/
char *re_read_window(void *session, const char *control, uint8_t *output, uint64_t capacity);
#ifdef __cplusplus
}
#endif
#endif
