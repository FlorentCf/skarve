/* Test-only C ABI driver: a guarded 128 KiB pthread makes a native stack
 * overflow deterministic instead of overwriting an adjacent FFI mapping.
 * This is not an engine execution strategy or a shipped runtime component. */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct Context {
    void *(*create)(void);
    char *(*call)(void *, const char *);
    void (*free_string)(char *);
    void (*drop)(void *);
    const char *source;
    const char *output;
    const char *codec;
    const char *predictor_json;
    const char *overview_json;
    const char *layout_json;
    unsigned int chunk_edge;
    int ordered;
    int success;
};

static int request(struct Context *context, void *handle, const char *json) {
    char *reply = context->call(handle, json);
    if (!reply) return 0;
    int success = strstr(reply, "\"ok\":true") != NULL;
    puts(reply);
    fflush(stdout);
    context->free_string(reply);
    return success;
}

static void *run(void *argument) {
    struct Context *context = argument;
    void *handle = context->create();
    char *json = calloc(8192, 1);
    if (!handle || !json) abort();
    snprintf(json, 8192,
             "{\"op\":\"register_source\",\"id\":\"guard\",\"spec\":{\"location\":\"%s\"%s}}",
             context->source, context->overview_json);
    if (!request(context, handle, json)) goto complete;
    snprintf(json, 8192,
             "{\"op\":\"compile_source\",\"source\":\"guard\",\"output\":\"%s\","
             "\"options\":{\"chunk_edge\":%u,\"codec\":\"%s\"%s%s}}",
             context->output, context->chunk_edge, context->codec, context->predictor_json, context->layout_json);
    if (!request(context, handle, json)) goto complete;
    if (!request(context, handle, "{\"op\":\"close_source\",\"source\":\"guard\"}")) goto complete;
    snprintf(json, 8192,
             "{\"op\":\"verify_skv\",\"spec\":{\"location\":\"%s\"}}",
             context->output);
    if (!request(context, handle, json)) goto complete;
    snprintf(json, 8192,
             "{\"op\":\"register_source\",\"id\":\"guard\",\"spec\":{\"location\":\"%s\"}}",
             context->output);
    if (!request(context, handle, json)) goto complete;
    if (!request(context, handle,
                 "{\"op\":\"measure_source\",\"source\":\"guard\",\"crs\":\"EPSG:3857\","
                 "\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[[[0,0],[16,0],[16,16],[0,16],[0,0]]]},"
                 "\"statistics\":[\"sum\",\"support\"]}")) goto complete;
    if (context->ordered && !request(context, handle,
                 "{\"op\":\"measure_ordered_source\",\"source\":\"guard\","
                 "\"numerical_policy\":\"hm_demographics_ordered_v1\","
                 "\"request\":{\"polygons\":[{\"id\":\"ordered\",\"windows\":["
                 "{\"window\":[0,0,16,16],\"runs\":[[0,256]]}]}]}}")) goto complete;
    context->success = request(context, handle, "{\"op\":\"close_source\",\"source\":\"guard\"}");
complete:
    free(json);
    context->drop(handle);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 5 || argc > 9) return 2;
    /* Only generated test paths are accepted; avoid ad-hoc JSON escaping. */
    for (int i = 2; i <= 3; i++) {
        if (strlen(argv[i]) > 3000 || strpbrk(argv[i], "\"\\\n\r")) return 2;
    }
    if (strcmp(argv[4], "none") && strcmp(argv[4], "deflate")) return 2;
    if (argc >= 6 && strcmp(argv[5], "none") && strcmp(argv[5], "byte_delta_v1")) return 2;
    char overview[32] = "";
    if (argc >= 8 && strcmp(argv[7], "ordered") && strcmp(argv[7], "none")) return 2;
    if (argc == 9 && strcmp(argv[8], "band") && strcmp(argv[8], "row_group_v1")) return 2;
    if (argc >= 7 && strcmp(argv[6], "none")) {
        if (!*argv[6] || strlen(argv[6]) > 2 || strspn(argv[6], "0123456789") != strlen(argv[6])) return 2;
        unsigned long value = strtoul(argv[6], NULL, 10);
        if (value >= 64) return 2;
        snprintf(overview, sizeof(overview), ",\"overview\":%lu", value);
    }
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) { fprintf(stderr, "%s\n", dlerror()); return 2; }
    struct Context context = {
        .create = dlsym(library, "re_new"),
        .call = dlsym(library, "re_call"),
        .free_string = dlsym(library, "re_free_string"),
        .drop = dlsym(library, "re_drop"),
        .source = argv[2], .output = argv[3], .codec = argv[4],
        .predictor_json = argc >= 6 && !strcmp(argv[5], "byte_delta_v1")
            ? ",\"predictor\":\"byte_delta_v1\"" : "",
        .overview_json = overview,
        .layout_json = argc == 9 && !strcmp(argv[8], "row_group_v1")
            ? ",\"payload_layout\":\"row_group_v1\",\"band_group\":64" : "",
        .chunk_edge = argc == 9 && !strcmp(argv[8], "row_group_v1") ? 128 : 64,
        .ordered = argc >= 8 && !strcmp(argv[7], "ordered"),
    };
    if (!context.create || !context.call || !context.free_string || !context.drop) return 2;
    pthread_attr_t attributes;
    pthread_t worker;
    if (pthread_attr_init(&attributes) ||
        pthread_attr_setstacksize(&attributes, 128 * 1024) ||
        pthread_attr_setguardsize(&attributes, (size_t)sysconf(_SC_PAGESIZE)) ||
        pthread_create(&worker, &attributes, run, &context)) return 2;
    pthread_attr_destroy(&attributes);
    if (pthread_join(worker, NULL)) return 2;
    dlclose(library);
    return context.success ? 0 : 1;
}
