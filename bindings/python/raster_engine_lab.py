"""Thin local ctypes binding. The CDLL releases the GIL during native calls.

One active call per session, no hidden request queue or internal thread pool.
Use await call_async() in event-loop applications. Large arrays are loaded once;
JSON registration and serialization/copy costs remain explicit.
"""
from __future__ import annotations

import asyncio
import ctypes
import json
import os
from pathlib import Path
import threading
import time


class EngineError(RuntimeError):
    pass


RUNTIME_HELP = ("Skarve Linux packages support Ubuntu 24.04 x86-64 with GDAL 3.8.4 and libdeflate 1.19. "
                "Install the runtime with: sudo apt-get update && sudo apt-get install "
                "libgdal34t64=3.8.4+dfsg-3ubuntu3 gdal-data=3.8.4+dfsg-3ubuntu3 libdeflate0=1.19-1build1.1. "
                "The Skarve core is bundled; no development-library path is required.")


def resolve_library(library=None):
    selected = library or os.environ.get("SKARVE_LIBRARY") or os.environ.get("RASTER_ENGINE_LIB")
    if selected:
        return Path(selected)
    installed = Path(__file__).resolve().parent / "skarve/_native/libraster_engine.so"
    if installed.is_file():
        return installed
    checkout = Path(__file__).resolve().parents[2] / "target/release/libraster_engine.so"
    if checkout.is_file():
        return checkout
    raise EngineError("Skarve native core was not found. Install the complete Linux wheel. " + RUNTIME_HELP)


def load_library(library=None):
    path = resolve_library(library)
    try:
        return ctypes.CDLL(str(path))
    except OSError as error:
        raise EngineError(f"Skarve could not load its native runtime ({error}). {RUNTIME_HELP}") from error


class Source:
    """An explicitly owned registered reader; the containing Engine must stay open."""
    def __init__(self, engine, source_id, metadata):
        self.engine, self.id, self.metadata = engine, source_id, metadata
        self.closed = False

    def _ensure_open(self):
        if self.closed:
            raise EngineError("This handle is closed; open a new handle before use.")

    def inspect(self):
        self._ensure_open()
        return self.engine.inspect_source(self.id)

    def measure(self, geometry, crs, **options):
        self._ensure_open()
        return self.engine.measure_source(geometry, crs, source=self.id, **options)

    def _window_request(self, window, bands, max_bytes, working_bytes):
        import sys
        self._ensure_open()
        if sys.byteorder != "little":
            raise EngineError("Typed window binding requires a little-endian host")
        raw = self.metadata.get("rawMetadata")
        if not raw:
            raise EngineError("Source has no typed metadata; reopen with a compatible runtime")
        if len(window) != 4 or any(type(n) is not int or n < 0 for n in window) or not window[2] or not window[3]:
            raise ValueError("Invalid window")
        if not bands or len(bands) > raw.get("maxWindowBands", raw["maxReadBands"]) or any(type(n) is not int or n < 0 or n >= len(raw["bands"]) for n in bands):
            raise ValueError("Invalid bands")
        if type(max_bytes) is not int or not 0 < max_bytes <= 64 << 20 or type(working_bytes) is not int or not 0 < working_bytes <= 128 << 20:
            raise ValueError("Window budget exceeds cap")
        widths = dict(byte=1, int8=1, uint16=2, int16=2, uint32=4, int32=4, float32=4, float64=8)
        cells, size = window[2] * window[3], 0
        for i in bands:
            size = (size + 7) // 8 * 8 + cells * (widths[raw["bands"][i]["scalarType"]] + 1)
        if size > max_bytes:
            raise ValueError("Window output budget exceeded")
        return dict(source=self.id, window=list(window), bands=list(bands), working_bytes=working_bytes), size

    def read_window(self, window, bands, *, max_bytes=64 << 20, working_bytes=128 << 20):
        """Original scalar memoryviews and independent mask bytes; no normalization."""
        request, size = self._window_request(window, bands, max_bytes, working_bytes)
        return self.engine.call(request, _binary=size)

    async def read_window_async(self, window, bands, *, max_bytes=64 << 20, working_bytes=128 << 20):
        request, size = self._window_request(window, bands, max_bytes, working_bytes)
        return await self.engine.call_async(request, _binary=size)

    def sum_selected(self, request, *, numerical_policy):
        """Sum explicit ordered source selections under the declared policy.

        Selection indexes/runs address this source's exposed grid. This expert
        interface does not infer geometry, overviews or population scaling.
        """
        self._ensure_open()
        return self.engine.sum_selected_source(request, source=self.id,
                                               numerical_policy=numerical_policy)

    async def sum_selected_async(self, request, *, numerical_policy):
        self._ensure_open()
        return await self.engine.call_async(dict(op="measure_ordered_source", source=self.id,
                                                 request=request, numerical_policy=numerical_policy))

    def carve(self, zone, *, bands=None, metrics=None, backend=None, crs=None, **options):
        """Query a zone already in this source's CRS; no reprojection is implied.

        Band indices address the registered source mapping. The native engine
        validates backend eligibility and returns the executed policy/provenance.
        """
        self._ensure_open()
        return self.measure(zone, self._carve_crs(crs),
                            **_carve_options(bands, metrics, backend, options))

    async def carve_async(self, zone, *, bands=None, metrics=None, backend=None, crs=None, **options):
        """Off-thread carve; cancellation drains native work before returning."""
        self._ensure_open()
        return await self.engine.call_async(dict(
            op="measure_source", source=self.id, geometry=zone, crs=self._carve_crs(crs),
            **_carve_options(bands, metrics, backend, options)))

    def _carve_crs(self, crs):
        if crs is not None:
            return crs
        return self.metadata["metadata"]["grid"]["crs"]

    def prepare(self, index, **options):
        self._ensure_open()
        return self.engine.prepare_source(index, source=self.id, **options)

    def ward(self, index, **options):
        """Build optional source-bound preparation; ordinary queries need none."""
        return self.prepare(index, **options)

    def compile(self, output, **options):
        """Compile this TIFF/COG snapshot into experimental self-contained SKV.

        Conversion preserves the registered band mapping and rejects overwrites.
        It is explicit preparation; ordinary TIFF/COG querying remains available.
        """
        self._ensure_open()
        return self.engine.compile_source(output, source=self.id, **options)

    async def compile_async(self, output, **options):
        """Off-thread compilation; cancellation drains and cleans partial output."""
        self._ensure_open()
        return await self.engine.call_async(dict(
            op="compile_source", source=self.id, output=str(output), options=options))

    def open_index(self, index, *, id="index", **options):
        self._ensure_open()
        return self.engine.open_index(index, source=self.id, id=id, **options)

    def close(self):
        if not self.closed:
            self.engine.close_source(self.id)
            self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class Index:
    """An explicitly retained validated index handle, bound to its source reader."""
    def __init__(self, engine, source_id, index_id, metadata):
        self.engine, self.source_id, self.id, self.metadata = engine, source_id, index_id, metadata
        self.closed = False

    def _ensure_open(self):
        if self.closed:
            raise EngineError("This handle is closed; open a new handle before use.")

    def inspect(self):
        self._ensure_open()
        return self.engine.index_info(self.id)

    def measure(self, geometry, crs, **options):
        self._ensure_open()
        return self.engine.measure_source(geometry, crs, source=self.source_id, index_handle=self.id, **options)

    def close(self):
        if not self.closed:
            self.engine.close_index(self.id)
            self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class Engine:
    def __init__(self, library=None):
        self._lib = load_library(library)
        self._lib.re_new.restype = ctypes.c_void_p
        self._lib.re_call.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        self._lib.re_call.restype = ctypes.c_void_p
        self._lib.re_cancel.argtypes = [ctypes.c_void_p]
        self._lib.re_free_string.argtypes = [ctypes.c_void_p]
        self._lib.re_drop.argtypes = [ctypes.c_void_p]
        self._handle = self._lib.re_new()
        self._guard = threading.Lock()
        self._state = threading.Lock()
        self._active_async_request = None
        self.last_call_profile = None
        self._source_sequence = 0
        if not self._handle:
            raise EngineError("native allocation failed")

    def call(self, request, *, _abort=None, _native_started=None, _binary=None):
        if not self._guard.acquire(blocking=False):
            raise EngineError("session busy; use separate bounded sessions")
        try:
            with self._state:
                if not self._handle:
                    raise EngineError("engine is closed")
                handle = self._handle
            if _abort is not None and _abort.is_set():
                raise EngineError("Request cancelled before native admission")
            call_start = time.perf_counter()
            encoded = json.dumps(request, separators=(",", ":"), allow_nan=False).encode()
            if len(encoded) > 64 * 1024 * 1024:
                raise EngineError("request exceeds 64 MiB")
            encode_end = time.perf_counter()
            if _abort is not None and _abort.is_set():
                raise EngineError("Request cancelled before native admission")
            if _native_started is not None:
                with self._state:
                    self._active_async_request = _native_started
                    _native_started.set()
            binary = None
            if _binary is not None:
                if type(_binary) is not int or not 0 < _binary <= 64 << 20:
                    raise EngineError("Window output capacity exceeds cap")
                binary = bytearray(_binary)
                function = self._lib.re_read_window
                function.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_uint64]
                function.restype = ctypes.c_void_p
                storage = (ctypes.c_ubyte * len(binary)).from_buffer(binary)
                pointer = function(handle, encoded, storage, len(binary))
            else:
                pointer = self._lib.re_call(handle, encoded)
            native_end = time.perf_counter()
            if not pointer:
                raise EngineError("native returned null")
            try:
                raw = ctypes.string_at(pointer)
                transfer_end = time.perf_counter()
                response = json.loads(raw)
                parse_end = time.perf_counter()
            finally:
                self._lib.re_free_string(pointer)
            self.last_call_profile = dict(op=request.get("op"), request_encode_ms=(encode_end-call_start)*1000,
                native_call_ms=(native_end-encode_end)*1000, ffi_copy_ms=(transfer_end-native_end)*1000,
                binding_parse_ms=(parse_end-transfer_end)*1000, free_ms=(time.perf_counter()-parse_end)*1000,
                response_bytes=len(raw), native=response.get("native_timing"), native_request_ms=response.get("request_ms"))
            if not response["ok"]:
                raise EngineError(response["error"])
            if binary is not None:
                formats = dict(byte="B", int8="b", uint16="H", int16="h", uint32="I", int32="i", float32="f", float64="d")
                for band in response["result"]["bands"]:
                    offset, size = band["byteOffset"], band["byteLength"]
                    band["values"] = memoryview(binary)[offset:offset+size].cast(formats[band["scalarType"]])
                    offset, size = band["maskOffset"], band["maskLength"]
                    band["mask"] = memoryview(binary)[offset:offset+size]
            return response["result"]
        finally:
            with self._state:
                self._active_async_request = None
            self._guard.release()

    async def call_async(self, request, *, _binary=None):
        # Shield preserves the worker task so native work finishes before callers
        # can safely destroy its handle after a cancellation.
        abort, native_started = threading.Event(), threading.Event()
        pending = asyncio.create_task(asyncio.to_thread(
            self.call, request, _abort=abort, _native_started=native_started, _binary=_binary))
        try:
            return await asyncio.shield(pending)
        except asyncio.CancelledError:
            abort.set()
            # Drain even after repeated task cancellation. A queued or rejected
            # busy request must not cancel some other request using the session.
            while not pending.done():
                if native_started.is_set():
                    with self._state:
                        if self._active_async_request is native_started and self._handle:
                            self._lib.re_cancel(self._handle)
                try:
                    await asyncio.wait_for(asyncio.shield(pending), timeout=0.01)
                except (asyncio.TimeoutError, asyncio.CancelledError):
                    continue
                except Exception:
                    break
            if pending.done() and not pending.cancelled():
                pending.exception()
            raise

    def bulk_reduce(self, windows, *, policy="strict_selected_v1", reducers=("sum",),
                    max_payload_bytes=128*1024*1024, max_contributions=268435456):
        """Reduce typed selected windows with owned bounded NumPy snapshots."""
        from skarve_bulk import reduce
        return reduce(self, windows, policy=policy, reducers=reducers,
                      max_payload_bytes=max_payload_bytes, max_contributions=max_contributions)

    async def bulk_reduce_async(self, windows, **options):
        """Off-thread binary reduction; cancellation waits for native cleanup."""
        from skarve_bulk import reduce_async
        return await reduce_async(self, windows, **options)

    def cancel(self):
        with self._state:
            if self._handle:
                self._lib.re_cancel(self._handle)

    def close(self):
        if not self._guard.acquire(blocking=False):
            raise EngineError("cannot close while call is active; cancel and await it")
        try:
            with self._state:
                if self._handle:
                    self._lib.re_drop(self._handle)
                    self._handle = None
        finally:
            self._guard.release()

    def open_raster(self, source, *, id="r"):
        return self.call({"op": "open", "id": id, **({"raster": source} if isinstance(source, dict) else {"path": str(source)})})

    def compile_polygon(self, geometry, crs, *, source="r", id="p"):
        return self.call(dict(op="compile", source=source, id=id, geometry=geometry, crs=crs))

    def measure(self, geometry=None, *, source="r", crs=None, plan=None, **options):
        selection = {"plan": plan} if plan is not None else {"geometry": geometry, "crs": crs}
        return self.call(dict(op="measure", source=source, **selection, **options))

    def prepare(self, source="r", **options):
        return self.call(dict(op="prepare", source=source, **options))

    def prepare_file(self, path, index, **options):
        return self.call(dict(op="prepare_file", path=str(path), index=str(index), **options))

    def register_source(self, spec, *, id="source"):
        return self.call(dict(op="register_source", id=id, spec=spec))

    def open_source(self, spec, *, id="source"):
        """Register a file/HTTPS/scientific source and return an owned reader handle."""
        if isinstance(spec, (str, Path)):
            spec = {"location": str(spec)}
        return Source(self, id, self.register_source(spec, id=id))

    def infuse(self, source, *, id=None):
        """Open a source with session ownership; an omitted ID is generated."""
        if id is None:
            self._source_sequence += 1
            id = f"__skarve_source_{self._source_sequence}"
        return self.open_source(source, id=id)

    def inspect_source(self, source="source"):
        return self.call(dict(op="source_info", source=source))

    def close_source(self, source="source"):
        return self.call(dict(op="close_source", source=source))

    def measure_source(self, geometry, crs, *, source="source", **options):
        return self.call(dict(op="measure_source", source=source, geometry=geometry, crs=crs, **options))

    def sum_selected_source(self, request, *, source="source", numerical_policy):
        return self.call(dict(op="measure_ordered_source", source=source, request=request,
                              numerical_policy=numerical_policy))

    def sum_selected(self, profile, request, *, numerical_policy, view_id, access_class="unknown"):
        """Select one attested representation and execute an ordered request.

        The profile is an owner-provisioned equivalence and performance contract.
        Selection uses the actual request; unknown access conditions use its
        direct source. Identity or execution failures never retry another source.
        """
        return self.call(dict(op="measure_ordered_profile", profile=profile, request=request,
                              numerical_policy=numerical_policy, view_id=view_id,
                              access_class=access_class))

    async def sum_selected_async(self, profile, request, *, numerical_policy, view_id,
                                 access_class="unknown"):
        return await self.call_async(dict(op="measure_ordered_profile", profile=profile,
                                         request=request, numerical_policy=numerical_policy,
                                         view_id=view_id, access_class=access_class))

    def register_index(self, index, *, source="source", id="index", **options):
        return self.call(dict(op="register_index", source=source, id=id, index=str(index), **options))

    def open_index(self, index, *, source="source", id="index", **options):
        return Index(self, source, id, self.register_index(index, source=source, id=id, **options))

    def index_info(self, id="index"):
        return self.call(dict(op="index_info", id=id))

    def close_index(self, id="index"):
        return self.call(dict(op="close_index", id=id))

    def measure_hm_population(self, geometry, *, source="source", **options):
        """Expert HM compatibility mode, separate from ordinary planar measure."""
        return self.call(dict(op="measure_hm_population", source=source, geometry=geometry,
                              mode="hm_straight_lonlat_spherical_v1", **options))

    def prepare_source(self, index, *, source="source", **options):
        return self.call(dict(op="prepare_source", source=source, index=str(index), **options))

    def compile_source(self, output, *, source="source", **options):
        return self.call(dict(op="compile_source", source=source, output=str(output), options=options))

    def verify_skv(self, source):
        """Read and validate every SKV payload under the declared source budgets."""
        if isinstance(source, (str, Path)):
            source = {"location": str(source), "format": "skv"}
        else:
            source = {"format": "skv", **source}
        return self.call(dict(op="verify_skv", spec=source))

    async def verify_skv_async(self, source):
        if isinstance(source, (str, Path)):
            source = {"location": str(source), "format": "skv"}
        else:
            source = {"format": "skv", **source}
        return await self.call_async(dict(op="verify_skv", spec=source))

    def batch_pages(self, job, *, id="batch", max_rows=128, checkpoint=None, checkpoint_interval=1):
        """Backpressured native pages. Persist page['checkpoint'] after consuming rows.

        Native jobs perform shared tile scans. Optional exactextract jobs stage
        a bounded complete upstream result before presenting pages. This iterator
        only requests pages; it never calls the single-polygon operation.
        With checkpoint_interval=N, intermediate pages may omit "checkpoint";
        every Nth page and the final page include one. job_info can snapshot the
        current checkpoint without reading sources or advancing the cursor.
        """
        if isinstance(checkpoint_interval, bool) or not isinstance(checkpoint_interval, int) or checkpoint_interval < 1:
            raise ValueError("checkpoint_interval must be a positive integer")
        request=dict(op="start_job", id=id, job=job)
        if checkpoint is not None:
            request["checkpoint"]=checkpoint
        self.call(request)
        try:
            page_number=0
            while True:
                page_number+=1
                page=self.call(dict(op="next_job", id=id, max_rows=max_rows,
                                    include_checkpoint=page_number % checkpoint_interval == 0))
                yield page
                if page["complete"]:
                    break
        finally:
            self.call(dict(op="close_job", id=id))

    def batch(self, job, **options):
        for page in self.batch_pages(job, **options):
            yield from page["rows"]

    def cleave(self, job, **options):
        """Return bounded pages from the real shared batch engine.

        The job uses zones/slices/CRS and may use metrics as a short spelling of
        options.statistics. No geometry or pixel arrays are duplicated here.
        """
        return self.batch_pages(_cleave_job(job), **options)

    def measure_file(self, geometry, crs, *, path=None, index=None, **options):
        request=dict(op="measure_file", geometry=geometry, crs=crs, **options)
        if path is not None:
            request["path"]=str(path)
        if index is not None:
            request["index"]=str(index)
        return self.call(request)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def _carve_options(bands, metrics, backend, options):
    options = dict(options)
    if metrics is not None:
        if "statistics" in options:
            raise TypeError("Use metrics or statistics, not both")
        options["statistics"] = metrics
    if bands is not None:
        options["bands"] = bands
    if backend is not None:
        options["backend"] = backend
    return options


def _cleave_job(job):
    if "metrics" not in job:
        return job
    job = dict(job)
    options = dict(job.get("options", {}))
    if "statistics" in options:
        raise TypeError("Use metrics or options.statistics, not both")
    options["statistics"] = job.pop("metrics")
    job["options"] = options
    return job


Skarve = Engine
