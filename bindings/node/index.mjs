import koffi from 'koffi';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { prepareBulk, bulkMemoryStats } from './bulk.mjs';
export { BULK_ABI_VERSION, BULK_LIMITS, bulkMemoryStats } from './bulk.mjs';

const MAX_REQUEST_BYTES = 64 * 1024 * 1024;
const MAX_SESSIONS = 4;
let liveSessions = 0;
const libraries = new Map();

export class RasterEngineError extends Error {
  constructor(message, code = 'ENGINE_ERROR') { super(message); this.name = 'RasterEngineError'; this.code = code; }
}
const RUNTIME_HELP = 'Qualified binary runtime: Ubuntu 24.04 x86-64, GDAL 3.8.4 and libdeflate 1.19. Install with: sudo apt-get update && sudo apt-get install libgdal34t64=3.8.4+dfsg-3ubuntu3 gdal-data=3.8.4+dfsg-3ubuntu3 libdeflate0=1.19-1build1.1. The Skarve native core is bundled; no development-library path is required.';
const aborted = () => new RasterEngineError('Request cancelled', 'ABORTED');

function nativeApi(filename) {
  if (libraries.has(filename)) return libraries.get(filename);
  let lib;
  try { lib = koffi.load(filename); }
  catch (error) {
    throw new RasterEngineError(`Skarve could not load its native runtime: ${error.message}. ${RUNTIME_HELP}`, 'NATIVE_RUNTIME');
  }
  const api = {
    lib,
    create: lib.func('void *re_new(void)'),
    call: lib.func('void *re_call(void *session, const char *request)'),
    freeString: lib.func('void re_free_string(void *text)'),
    cancel: lib.func('void re_cancel(void *session)'),
    drop: lib.func('void re_drop(void *session)'),
  };
  libraries.set(filename, api);
  return api;
}

function resolveLibrary(explicit) {
  const filename = explicit || process.env.SKARVE_LIBRARY || process.env.RASTER_ENGINE_LIBRARY;
  if (filename) return filename;
  const installed = fileURLToPath(new URL('./native/linux-x64/libraster_engine.so', import.meta.url));
  if (process.platform === 'linux' && process.arch === 'x64' && existsSync(installed)) return installed;
  const platformName = process.platform === 'win32' ? 'raster_engine.dll' : process.platform === 'darwin' ? 'libraster_engine.dylib' : 'libraster_engine.so';
  const candidate = fileURLToPath(new URL(`../../target/release/${platformName}`, import.meta.url));
  if (!existsSync(candidate)) throw new RasterEngineError(`Skarve native core was not found. Install the complete Linux tarball. ${RUNTIME_HELP}`, 'MISSING_LIBRARY');
  return candidate;
}

/** Persistent native session. One active request per session; no hidden queue. */
export class RasterEngine {
  #api; #handle; #pending = null; #closed = false; #closing = null; #cancelRequest = null;
  #sourceSequence = 0;
  #readerMetadata = new Map();
  lastTiming = null;
  constructor({ library } = {}) {
    if (liveSessions >= MAX_SESSIONS) throw new RasterEngineError(`At most ${MAX_SESSIONS} Node sessions may be live`, 'SESSION_LIMIT');
    this.#api = nativeApi(resolveLibrary(library));
    this.#handle = this.#api.create();
    if (!this.#handle) throw new RasterEngineError('Native session allocation failed');
    liveSessions++;
  }
  get busy() { return this.#pending !== null; }
  get closed() { return this.#closed; }
  /** Selected original typed values; strict default, explicit HM ordered policy.
   * Snapshot ownership lasts through native completion, including abort/close.
   */
  async bulkReduce(windows, options = {}) {
    const { signal } = options;
    if (this.#closed) throw new RasterEngineError('Session is closed', 'CLOSED');
    if (this.busy) throw new RasterEngineError('Session is busy; await its current request', 'BUSY');
    if (signal?.aborted) throw aborted();
    if (!this.#api.bulk) {
      try { this.#api.bulk = this.#api.lib.func('int32_t re_bulk(void *session, const void *request, void *results, uint64_t result_capacity, void *metadata, void *error, uint64_t error_capacity)'); }
      catch (error) { throw new RasterEngineError(`This native artifact does not provide bulk ABI v1: ${error.message}`, 'BULK_UNAVAILABLE'); }
    }
    const started = performance.now();
    const snapshot = prepareBulk(windows, options);
    if (signal?.aborted) { snapshot.dispose(); throw aborted(); }
    const copied = performance.now();
    let returned = copied, decoded = copied, cancelRequested = false, cancelRepeater = null;
    const cancel = () => {
      cancelRequested = true;
      this.#api.cancel(this.#handle);
      // Covers abort while this call is still queued in the bounded FFI pool.
      cancelRepeater ??= setInterval(() => this.#api.cancel(this.#handle), 10);
      cancelRepeater.unref();
    };
    signal?.addEventListener('abort', cancel, { once: true });
    this.#cancelRequest = cancel;
    this.#pending = new Promise((resolve, reject) => {
      this.#api.bulk.async(this.#handle, ...snapshot.arguments, (error, status) => {
        returned = performance.now();
        try {
          if (error) throw error;
          if (signal?.aborted || cancelRequested) throw aborted();
          const result = snapshot.decode(status);
          decoded = performance.now();
          result.metadata.binding_timing_ms = { snapshot: copied - started, native_and_ffi: returned - copied, decode: decoded - returned, total: decoded - started };
          resolve(result);
        } catch (error) { reject(error instanceof RasterEngineError ? error : new RasterEngineError(error.message, error.code ?? 'BULK_ERROR')); }
      });
    });
    try { return await this.#pending; }
    finally {
      signal?.removeEventListener('abort', cancel);
      if (cancelRepeater !== null) clearInterval(cancelRepeater);
      snapshot.dispose();
      this.lastTiming = { op: 'bulk_reduce', snapshot: copied - started, native_and_ffi: returned - copied,
        decoding: Math.max(0, decoded - returned), copied_bytes: snapshot.payloadBytes, total: performance.now() - started,
        owned_bytes_after: bulkMemoryStats().owned_bytes };
      this.#pending = null; this.#cancelRequest = null;
    }
  }
  request(request, options) { return this.#execute(request, options); }
  async #execute(request, { signal } = {}, binary = null) {
    if (this.#closed) throw new RasterEngineError('Session is closed', 'CLOSED');
    if (this.busy) throw new RasterEngineError('Session is busy; await its current request', 'BUSY');
    if (signal?.aborted) throw aborted();
    const started = performance.now();
    const json = JSON.stringify(request);
    if (typeof json !== 'string' || Buffer.byteLength(json, 'utf8') > MAX_REQUEST_BYTES) throw new RasterEngineError('Request exceeds 64 MiB', 'REQUEST_LIMIT');
    const encoded = performance.now();
    let cancelRepeater = null; let cancelRequested = false;
    const cancel = () => {
      cancelRequested = true;
      this.#api.cancel(this.#handle);
      // Cover an abort that races before native re_call starts and clears its flag.
      cancelRepeater ??= setInterval(() => this.#api.cancel(this.#handle), 10);
      cancelRepeater.unref();
    };
    signal?.addEventListener('abort', cancel, { once: true });
    this.#cancelRequest = cancel;
    this.#pending = new Promise((resolve, reject) => {
      const operation = binary ? this.#api.readWindow : this.#api.call;
      const args = binary ? [this.#handle, json, binary, binary.length] : [this.#handle, json];
      operation.async(...args, (ffiError, pointer) => {
        const returned = performance.now();
        let transferred = returned, parsed = returned, nativeTiming = null, responseBytes = 0;
        try {
          if (ffiError) throw ffiError;
          if (!pointer) throw new RasterEngineError('Native request returned a null pointer');
          const text = koffi.decode.string(pointer);
          transferred = performance.now(); responseBytes = Buffer.byteLength(text);
          const envelope = JSON.parse(text);
          parsed = performance.now(); nativeTiming = envelope.native_timing ?? null;
          if (signal?.aborted || cancelRequested) throw aborted();
          if (!envelope.ok) throw new RasterEngineError(envelope.error || 'Native request failed');
          if (binary) {
            const types = { byte: Uint8Array, int8: Int8Array, uint16: Uint16Array, int16: Int16Array, uint32: Uint32Array, int32: Int32Array, float32: Float32Array, float64: Float64Array };
            for (const band of envelope.result.bands) {
              const Type = types[band.scalarType];
              band.values = new Type(binary.buffer, binary.byteOffset + band.byteOffset, band.byteLength / Type.BYTES_PER_ELEMENT);
              band.mask = new Uint8Array(binary.buffer, binary.byteOffset + band.maskOffset, band.maskLength);
            }
          }
          resolve(envelope.result);
        } catch (error) { reject(error); }
        finally {
          if (pointer) this.#api.freeString(pointer);
          this.lastTiming = { encoding: encoded - started, native_and_ffi: returned - encoded, decoding: performance.now() - returned, ffi_copy: transferred - returned, parsing: parsed - transferred, free: performance.now() - parsed, native: nativeTiming, response_bytes: responseBytes, total: performance.now() - started };
        }
      });
    });
    try { return await this.#pending; }
    finally {
      signal?.removeEventListener('abort', cancel);
      if (cancelRepeater !== null) clearInterval(cancelRepeater);
      this.#pending = null; this.#cancelRequest = null;
    }
  }
  open(id, rasterOrPath, options) {
    const source = typeof rasterOrPath === 'string' ? { path: rasterOrPath } : { raster: rasterOrPath };
    return this.request({ op: 'open', id, ...source }, options);
  }
  compile(source, id, geometry, crs, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'compile', source, id, geometry, crs }, { signal });
  }
  measure(source, geometryOrPlan, options = {}) {
    const { signal, ...nativeOptions } = options;
    const selection = typeof geometryOrPlan === 'string' ? { plan: geometryOrPlan } : { geometry: geometryOrPlan };
    return this.request({ ...nativeOptions, ...selection, op: 'measure', source }, { signal });
  }
  prepare(source, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'prepare', source }, { signal });
  }
  prepareFile(path, index, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'prepare_file', path, index }, { signal });
  }
  measureFile(geometry, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'measure_file', geometry }, { signal });
  }
  registerFile(id, path, options) { return this.request({ op: 'register_file', id, path }, options); }
  async registerSource(id, spec, options) { const result = await this.request({ op: 'register_source', id, spec }, options); this.#readerMetadata.set(id, structuredClone(result.rawMetadata)); return result; }
  async openSource(spec, { id = 'source', signal } = {}) {
    const metadata = await this.registerSource(id, typeof spec === 'string' ? { location: spec } : spec, { signal });
    return new Source(this, id, metadata);
  }
  infuse(source, { id, signal } = {}) {
    id ??= `__skarve_source_${++this.#sourceSequence}`;
    return this.openSource(source, { id, signal });
  }
  async closeReader(source, options) { const result = await this.request({ op: 'close_source', source }, options); this.#readerMetadata.delete(source); return result; }
  readWindowSource(source, request, options = {}) {
    if (this.#closed) throw new RasterEngineError('Session is closed', 'CLOSED');
    if (this.busy) throw new RasterEngineError('Session is busy', 'BUSY');
    if (options.signal?.aborted) throw aborted();
    if (new Uint8Array(new Uint16Array([1]).buffer)[0] !== 1) throw new Error('Typed window binding requires a little-endian host');
    if (!request || Object.keys(request).some(k => !['window', 'bands', 'maxBytes', 'workingBytes'].includes(k))) throw new TypeError('Unknown readWindow request field');
    const { window, bands, maxBytes = 64 << 20, workingBytes = 128 << 20 } = request;
    const raw = this.#readerMetadata.get(source);
    if (!raw) throw new Error('Source has no typed metadata; reopen with a compatible runtime');
    if (!Array.isArray(window) || window.length !== 4 || !window.every(n => Number.isSafeInteger(n) && n >= 0) || window[2] === 0 || window[3] === 0) throw new RangeError('Invalid window');
    // New native artifacts can partition a complete output window internally.
    // Older artifacts retain their declared per-read width.
    const maxWindowBands = raw.maxWindowBands ?? raw.maxReadBands;
    if (!Array.isArray(bands) || bands.length === 0 || bands.length > maxWindowBands || bands.length > 64 || new Set(bands).size !== bands.length || !bands.every(i => Number.isSafeInteger(i) && i >= 0 && i < raw.bands.length)) throw new RangeError('Invalid bands');
    if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > (64 << 20) || !Number.isSafeInteger(workingBytes) || workingBytes < 1 || workingBytes > (128 << 20)) throw new RangeError('Window budget exceeds cap');
    const widths = { byte: 1, int8: 1, uint16: 2, int16: 2, uint32: 4, int32: 4, float32: 4, float64: 8 };
    const cells = window[2] * window[3]; let bytes = 0;
    for (const band of bands) { bytes = Math.ceil(bytes / 8) * 8 + cells * (widths[raw.bands[band].scalarType] + 1); }
    if (!Number.isSafeInteger(bytes) || bytes > maxBytes) throw new RangeError('Window output budget exceeded');
    this.#api.readWindow ??= this.#api.lib.func('void *re_read_window(void *session, const char *control, void *output, uint64_t capacity)');
    const output = Buffer.alloc(bytes);
    return this.#execute({ source, window, bands, working_bytes: workingBytes }, options, output);
  }
  sourceInfo(source, options) { return this.request({ op: 'source_info', source }, options); }
  compileSource(source, output, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ op: 'compile_source', source, output, options: nativeOptions }, { signal });
  }
  verifySkv(source, options) {
    const spec = typeof source === 'string' ? { location: source, format: 'skv' } : { format: 'skv', ...source };
    return this.request({ op: 'verify_skv', spec }, options);
  }
  measureSource(source, geometry, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'measure_source', source, geometry }, { signal });
  }
  sumSelectedSource(source, request, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'measure_ordered_source', source, request }, { signal });
  }
  sumSelected(profile, request, options = {}) {
    const { signal, access_class = 'unknown', ...nativeOptions } = options;
    return this.request({ ...nativeOptions, access_class, op: 'measure_ordered_profile', profile, request }, { signal });
  }
  registerIndex(source, id, index, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'register_index', source, id, index }, { signal });
  }
  async openIndex(source, index, { id = 'index', ...options } = {}) {
    return new Index(this, source, id, await this.registerIndex(source, id, index, options));
  }
  indexInfo(id, options) { return this.request({ op: 'index_info', id }, options); }
  closeIndex(id, options) { return this.request({ op: 'close_index', id }, options); }
  measureHMPopulation(source, geometry, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'measure_hm_population', source, geometry, mode: 'hm_straight_lonlat_spherical_v1' }, { signal });
  }
  prepareSource(source, index, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'prepare_source', source, index }, { signal });
  }
  async *batchPages(job, { id = 'batch', maxRows = 128, checkpoint, checkpointInterval = 1, signal } = {}) {
    if (!Number.isSafeInteger(checkpointInterval) || checkpointInterval < 1) throw new RangeError('checkpointInterval must be a positive safe integer');
    await this.request({ op: 'start_job', id, job, ...(checkpoint ? { checkpoint } : {}) }, { signal });
    try {
      let pageNumber = 0;
      while (true) {
        pageNumber += 1;
        const page = await this.request({ op: 'next_job', id, max_rows: maxRows, include_checkpoint: pageNumber % checkpointInterval === 0 }, { signal });
        yield page;
        if (page.complete) break;
      }
    } finally {
      if (!this.closed) await this.request({ op: 'close_job', id });
    }
  }
  async *batch(job, options) {
    for await (const page of this.batchPages(job, options)) yield* page.rows;
  }
  cleave(job, options) {
    if (Object.hasOwn(job, 'metrics')) {
      if (Object.hasOwn(job.options ?? {}, 'statistics')) throw new TypeError('Use metrics or options.statistics, not both');
      const { metrics, ...rest } = job;
      job = { ...rest, options: { ...job.options, statistics: metrics } };
    }
    return this.batchPages(job, options);
  }
  configureFileCache(bytes, options) { return this.request({ op: 'configure_file_cache', bytes }, options); }
  measureRegisteredFile(source, geometry, options = {}) {
    const { signal, ...nativeOptions } = options;
    return this.request({ ...nativeOptions, op: 'measure_registered_file', source, geometry }, { signal });
  }
  clearFileCache(options) { return this.request({ op: 'clear_file_cache' }, options); }
  closeFile(source, options) { return this.request({ op: 'close_file', source }, options); }
  closeSource(source, options) { return this.request({ op: 'close', source }, options); }
  stats(options) { return this.request({ op: 'stats' }, options); }
  cancel() { if (!this.#closed && this.busy) this.#cancelRequest?.(); }
  async close() {
    if (this.#closing) return this.#closing;
    if (this.#closed) return;
    this.#closed = true;
    this.#closing = (async () => {
      if (this.#pending) { this.#cancelRequest?.(); await this.#pending.catch(() => {}); }
      this.#api.drop(this.#handle);
      this.#handle = null;
      liveSessions--;
    })();
    return this.#closing;
  }
  async [Symbol.asyncDispose]() { await this.close(); }
}

/** Owned registered reader. closeSource remains the historical resident-raster alias. */
export class Source {
  constructor(engine, id, metadata) { this.engine = engine; this.id = id; this.metadata = metadata; this.closed = false; }
  assertOpen() { if (this.closed) throw new RasterEngineError("This handle is closed; open a new handle before use.", "CLOSED_HANDLE"); }
  inspect(options) { this.assertOpen(); return this.engine.sourceInfo(this.id, options); }
  metrics(options) { this.assertOpen(); return this.engine.request({ op: 'source_metrics', source: this.id }, options); }
  beginQuery(options) { this.assertOpen(); return this.engine.request({ op: 'begin_source_query', source: this.id }, options); }
  _beginVerifiedQuery({ renewBudget = true, ...options } = {}) { this.assertOpen(); return this.engine.request({ op: 'begin_verified_source_query', source: this.id, renew_budget: renewBudget }, options); }
  _endVerifiedQuery(options) { this.assertOpen(); return this.engine.request({ op: 'end_verified_source_query', source: this.id }, options); }
  /** Callback values are provisional. Failure closes this source; only a
   * successful final generation check releases the callback's complete result. */
  async withVerifiedQuery(action, options = {}) {
    try {
      await this._beginVerifiedQuery(options);
      const result = await action(this);
      await this._endVerifiedQuery(options);
      return result;
    } catch (error) { await this.close(); throw error; }
  }
  readWindow(request, options) { this.assertOpen(); return this.engine.readWindowSource(this.id, request, options); }
  measure(geometry, options) { this.assertOpen(); return this.engine.measureSource(this.id, geometry, options); }
  sumSelected(request, options) { this.assertOpen(); return this.engine.sumSelectedSource(this.id, request, options); }
  carve({ zone, metrics, ...options }) {
    this.assertOpen();
    if (zone === undefined) throw new TypeError('carve requires zone');
    if (metrics !== undefined) {
      if (Object.hasOwn(options, 'statistics')) throw new TypeError('Use metrics or statistics, not both');
      options.statistics = metrics;
    }
    options.crs ??= this.metadata.metadata.grid.crs;
    return this.measure(zone, options);
  }
  prepare(index, options) { this.assertOpen(); return this.engine.prepareSource(this.id, index, options); }
  ward(index, options) { return this.prepare(index, options); }
  compile(output, options) { this.assertOpen(); return this.engine.compileSource(this.id, output, options); }
  openIndex(index, options) { this.assertOpen(); return this.engine.openIndex(this.id, index, options); }
  async close() { if (!this.closed) { await this.engine.closeReader(this.id); this.closed = true; } }
  async [Symbol.asyncDispose]() { await this.close(); }
}
export { RasterEngine as Skarve, RasterEngine as Engine, RasterEngineError as SkarveError };
export default RasterEngine;

export class Index {
  constructor(engine, source, id, metadata) { this.engine = engine; this.source = source; this.id = id; this.metadata = metadata; this.closed = false; }
  assertOpen() { if (this.closed) throw new RasterEngineError("This handle is closed; open a new handle before use.", "CLOSED_HANDLE"); }
  inspect(options) { this.assertOpen(); return this.engine.indexInfo(this.id, options); }
  measure(geometry, options = {}) { this.assertOpen(); return this.engine.measureSource(this.source, geometry, { ...options, index_handle: this.id }); }
  async close() { if (!this.closed) { await this.engine.closeIndex(this.id); this.closed = true; } }
  async [Symbol.asyncDispose]() { await this.close(); }
}
