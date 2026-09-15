"""Owned, bounded, dtype-preserving ctypes selected-window interface (ABI v1).

NumPy is optional until this operation is used. Inputs must remain stable while
the snapshot is being made; concurrent mutation during a copy cannot define an
atomic source generation. Native sees only owned snapshots. No buffer is
retained by the native engine, and transient labels are not source verification.
"""
from __future__ import annotations

import asyncio
import ctypes as C
import threading
import time
from dataclasses import dataclass

ABI = 1
MAX_BYTES = 128 * 1024 * 1024
MAX_SNAPSHOTS = 512 * 1024 * 1024
MAX_CONTRIBUTIONS = 268435456
MAX_BANDS, MAX_WINDOWS = 64, 4096
_POLICIES = {"strict_selected_v1": 1, "hm_demographics_ordered_v1": 2}
_REDUCERS = {"sum": 1, "min": 2, "max": 4, "mean": 8}
_RESERVED = 0
_RESERVATION_LOCK = threading.Lock()


class Band(C.Structure):
    _fields_ = [
        ("data", C.c_void_p), ("data_bytes", C.c_uint64),
        ("byte_offset", C.c_uint64), ("byte_stride", C.c_uint64),
        ("cell_count", C.c_uint64), ("validity", C.c_void_p),
        ("validity_bytes", C.c_uint64), ("validity_offset", C.c_uint64),
        ("nodata", C.c_double), ("band_id", C.c_uint32),
        ("dtype", C.c_uint32), ("validity_kind", C.c_uint32), ("flags", C.c_uint32),
    ]


class Selection(C.Structure):
    _fields_ = [("data", C.c_void_p), ("data_bytes", C.c_uint64),
                ("count", C.c_uint64), ("kind", C.c_uint32), ("flags", C.c_uint32)]


class Window(C.Structure):
    _fields_ = [("bands", C.POINTER(Band)), ("band_count", C.c_uint32),
                ("flags", C.c_uint32), ("selection", Selection)]


class Request(C.Structure):
    _fields_ = [("abi_version", C.c_uint32), ("struct_size", C.c_uint32),
                ("policy", C.c_uint32), ("reducers", C.c_uint32),
                ("windows", C.POINTER(Window)), ("window_count", C.c_uint64),
                ("max_payload_bytes", C.c_uint64), ("max_contributions", C.c_uint64),
                ("flags", C.c_uint64)]


class Result(C.Structure):
    _fields_ = [("band_id", C.c_uint32), ("flags", C.c_uint32),
                ("sum", C.c_double), ("min", C.c_double), ("max", C.c_double),
                ("mean", C.c_double), ("valid_count", C.c_uint64),
                ("excluded_mask", C.c_uint64), ("excluded_nodata", C.c_uint64),
                ("excluded_nonfinite", C.c_uint64), ("excluded_negative", C.c_uint64)]


class Metadata(C.Structure):
    _fields_ = [("abi_version", C.c_uint32), ("struct_size", C.c_uint32)] + [
        (name, C.c_uint64) for name in ("window_count", "band_count", "payload_bytes",
        "selection_bytes", "result_bytes", "native_owned_bytes", "validation_ns", "reduction_ns")]


def _layout_check():
    if C.sizeof(C.c_void_p) != 8 or [C.sizeof(t) for t in
            (Band, Selection, Window, Request, Result, Metadata)] != [88, 32, 48, 56, 80, 72]:
        raise RuntimeError("bulk ABI v1 requires the verified 64-bit native layout")


def reserved_snapshot_bytes():
    """Process-wide currently reserved owned snapshots and bounded control state."""
    with _RESERVATION_LOCK:
        return _RESERVED


def _reserve(size, error):
    global _RESERVED
    with _RESERVATION_LOCK:
        if size > MAX_SNAPSHOTS - _RESERVED:
            raise error("process bulk snapshot budget exceeded")
        _RESERVED += size


def _release(size):
    global _RESERVED
    with _RESERVATION_LOCK:
        _RESERVED -= size
        assert _RESERVED >= 0


def _integer(value, minimum, maximum, label, error):
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise error(f"{label} must be an integer in [{minimum}, {maximum}]")
    return value


@dataclass(frozen=True)
class BandSpec:
    id: int
    values: object
    mask: object
    mask_kind: int
    mask_offset: int
    nodata: float
    flags: int


@dataclass(frozen=True)
class WindowSpec:
    bands: tuple
    selection: object
    selection_kind: int
    selected_count: int


def _control_charge(windows, error):
    if not isinstance(windows, (list, tuple)) or not 1 <= len(windows) <= MAX_WINDOWS:
        raise error("windows must be a bounded nonempty list/tuple")
    descriptors = 0
    for window in windows:
        if not isinstance(window, dict):
            raise error("window must be a mapping")
        bands = window.get("bands")
        if not isinstance(bands, (list, tuple)) or not 1 <= len(bands) <= MAX_BANDS:
            raise error("each window requires 1..64 bands")
        descriptors += len(bands)
    count = len(windows)
    # Reserve before allocating normalized control objects. This upper bound
    # charges a validity owner per band and a selection owner per window even
    # when absent, plus separate NumPy allocation/view headers.
    return (count*C.sizeof(Window) + descriptors*C.sizeof(Band) + C.sizeof(Request)
            + MAX_BANDS*C.sizeof(Result) + C.sizeof(Metadata) + 512 + 4096
            + descriptors*512 + count*1024 + (descriptors*2+count)*256)


def _preflight(windows, policy, reducers, max_payload_bytes, max_contributions, controls, error):
    try:
        import numpy as np
    except ImportError as exc:
        raise error("bulk_reduce requires NumPy; install NumPy in this environment") from exc
    if not isinstance(policy, str) or policy not in _POLICIES:
        raise error("unknown bulk numerical policy")
    if not isinstance(reducers, (list, tuple)) or not reducers or any(not isinstance(v, str) for v in reducers) or len(set(reducers)) != len(reducers):
        raise error("reducers must be a nonempty unique list/tuple")
    if any(not isinstance(item, str) or item not in _REDUCERS for item in reducers):
        raise error("unsupported bulk reducer")
    _integer(max_payload_bytes, 1, MAX_BYTES, "max_payload_bytes", error)
    _integer(max_contributions, 1, MAX_CONTRIBUTIONS, "max_contributions", error)
    if not isinstance(windows, (list, tuple)) or not 1 <= len(windows) <= MAX_WINDOWS:
        raise error("windows must be a bounded nonempty list/tuple")

    def array(value, dtypes, label):
        if not isinstance(value, np.ndarray) or value.dtype not in dtypes or not value.dtype.isnative:
            raise error(f"{label} must be a native-endian NumPy array of the documented dtype")
        if not value.flags.c_contiguous:
            raise error(f"{label} is noncontiguous; make an explicit charged contiguous copy first")
        return value

    planned, identity = [], None
    payload = contributions = descriptor_count = array_count = 0
    for window in windows:
        if not isinstance(window, dict) or set(window) - {"bands", "selection"}:
            raise error("window accepts only bands and selection")
        incoming = window.get("bands")
        if not isinstance(incoming, (list, tuple)) or not 1 <= len(incoming) <= MAX_BANDS:
            raise error("each window requires 1..64 bands")
        specs, ids, cells = [], [], None
        for band in incoming:
            if not isinstance(band, dict) or set(band) - {
                    "id", "values", "nodata", "validity", "validity_kind", "validity_offset"}:
                raise error("unknown band field")
            band_id = _integer(band.get("id"), 0, 2**32-1, "band id", error)
            values = array(band.get("values"), (np.dtype("float32"), np.dtype("float64")), "values")
            if cells is None:
                cells = int(values.size)
            elif values.size != cells:
                raise error("band cell counts differ within a window")
            mask = band.get("validity")
            kind = band.get("validity_kind", "bytes" if mask is not None else "all")
            if kind not in ("all", "bytes", "bits"):
                raise error("unknown validity kind")
            offset = _integer(band.get("validity_offset", 0), 0, 2**64-1, "validity offset", error)
            if mask is None:
                if kind != "all" or offset:
                    raise error("absent validity requires all and zero offset")
                mask_kind = 0
            else:
                mask = array(mask, (np.dtype("uint8"),), "validity")
                if kind == "all":
                    raise error("all-valid band must omit validity")
                mask_kind = 1 if kind == "bytes" else 2
                required = offset + cells
                if required > 2**64-1 or (required if mask_kind == 1 else (required+7)//8) > mask.size:
                    raise error("validity is shorter than offset and cell count")
                payload += int(mask.nbytes)
                array_count += 1
            flags = int("nodata" in band)
            try:
                nodata = float(band.get("nodata", 0.0))
            except (TypeError, ValueError, OverflowError) as exc:
                raise error("nodata must be a numeric scalar") from exc
            payload += int(values.nbytes)
            array_count += 1
            descriptor_count += 1
            ids.append(band_id)
            specs.append(BandSpec(band_id, values, mask, mask_kind, offset, nodata, flags))
        if len(set(ids)) != len(ids) or (identity is not None and tuple(ids) != identity):
            raise error("band identities must be unique and identically ordered across windows")
        identity = tuple(ids)
        selected = window.get("selection")
        selection_kind = 0
        count = cells
        if selected is not None:
            if isinstance(selected, dict):
                if set(selected) != {"spans"}:
                    raise error("span selection accepts only spans")
                selected = array(selected["spans"], (np.dtype("uint64"),), "spans")
                if selected.ndim != 2 or selected.shape[1] != 2:
                    raise error("spans must have shape (n, 2)")
                selection_kind = 3
                # Metadata-sized traversal avoids a temporary uint64 sum/cumsum
                # and detects wrap before the native byte/selection validation.
                count = 0
                for start, length in selected:
                    start, length = int(start), int(length)
                    if start > cells or length > cells-start:
                        raise error("span is outside the supplied window")
                    count += length
                    if count > max_contributions:
                        raise error("bulk contribution budget exceeded")
            else:
                selected = array(selected, (np.dtype("uint32"), np.dtype("uint64")), "selection")
                if selected.ndim != 1:
                    raise error("indices must be one-dimensional")
                selection_kind = 1 if selected.dtype.itemsize == 4 else 2
                count = int(selected.size)
            payload += int(selected.nbytes)
            array_count += 1
        contributions += count * len(specs)
        if contributions > max_contributions:
            raise error("bulk contribution budget exceeded")
        if payload > max_payload_bytes:
            raise error("bulk payload byte budget exceeded")
        planned.append(WindowSpec(tuple(specs), selected, selection_kind, count))

    # Conservative explicit allowance for Python owner/spec objects and ctypes
    # wrappers, in addition to their exact numeric allocations. Caller-owned
    # source arrays are separate and remain part of the application process cap.
    charge = payload + controls
    if charge > max_payload_bytes:
        raise error("bulk payload plus descriptor/owner state exceeds per-call budget")
    return np, tuple(planned), identity, payload, charge, sum(_REDUCERS[name] for name in reducers)


def _copy_array(np, source, owners):
    if source is None:
        return None
    copy = np.array(source, dtype=source.dtype, order="C", copy=True, subok=False).reshape(-1)
    copy.flags.writeable = False
    owners.append(copy)
    return copy


def reduce(engine, windows, *, policy="strict_selected_v1", reducers=("sum",),
           max_payload_bytes=MAX_BYTES, max_contributions=MAX_CONTRIBUTIONS,
           _abort=None, _native_started=None):
    from raster_engine_lab import EngineError
    if not engine._guard.acquire(blocking=False):
        raise EngineError("session busy; use separate bounded sessions")
    reserved = 0
    owners, band_arrays = [], []
    started = time.perf_counter()
    try:
        with engine._state:
            if not engine._handle:
                raise EngineError("engine is closed")
            handle = engine._handle
        _layout_check()
        if _abort is not None and _abort.is_set():
            raise EngineError("cancelled before bulk snapshot")
        _integer(max_payload_bytes, 1, MAX_BYTES, "max_payload_bytes", EngineError)
        controls = _control_charge(windows, EngineError)
        if controls > max_payload_bytes:
            raise EngineError("bulk descriptor/owner state exceeds per-call budget")
        _reserve(controls, EngineError)
        reserved = controls
        np, plan, ids, payload, charge, reducers_bits = _preflight(
            windows, policy, reducers, max_payload_bytes, max_contributions, controls, EngineError)
        preflight_end = time.perf_counter()
        _reserve(payload, EngineError)
        reserved += payload
        native_windows = (Window * len(plan))()
        for wi, win in enumerate(plan):
            native_bands = (Band * len(win.bands))()
            band_arrays.append(native_bands)
            for bi, spec in enumerate(win.bands):
                if _abort is not None and _abort.is_set():
                    raise EngineError("cancelled during bulk snapshot")
                values = _copy_array(np, spec.values, owners)
                mask = _copy_array(np, spec.mask, owners)
                native_bands[bi] = Band(
                    values.ctypes.data, values.nbytes, 0, values.dtype.itemsize, values.size,
                    0 if mask is None else mask.ctypes.data,
                    0 if mask is None else mask.nbytes, spec.mask_offset, spec.nodata,
                    spec.id, 1 if values.dtype.itemsize == 4 else 2, spec.mask_kind, spec.flags)
            selected = _copy_array(np, win.selection, owners)
            selection_count = 0 if selected is None else selected.size // (2 if win.selection_kind == 3 else 1)
            native_windows[wi] = Window(native_bands, len(win.bands), 0,
                Selection(0 if selected is None else selected.ctypes.data,
                          0 if selected is None else selected.nbytes,
                          selection_count, win.selection_kind, 0))
        output = (Result * len(ids))()
        meta = Metadata()
        error = C.create_string_buffer(512)
        req = Request(ABI, C.sizeof(Request), _POLICIES[policy], reducers_bits, native_windows,
                      len(plan), max_payload_bytes, max_contributions, 0)
        try:
            function = engine._lib.re_bulk
        except AttributeError as exc:
            raise EngineError("native library does not support bulk ABI v1; install the matching package") from exc
        function.argtypes = [C.c_void_p, C.POINTER(Request), C.POINTER(Result), C.c_uint64,
                             C.POINTER(Metadata), C.c_void_p, C.c_uint64]
        function.restype = C.c_int32
        snapshot_end = time.perf_counter()
        if _abort is not None and _abort.is_set():
            raise EngineError("cancelled before native bulk execution")
        if _native_started is not None:
            _native_started.set()
        status = function(handle, C.byref(req), output, len(ids), C.byref(meta), error, len(error))
        native_end = time.perf_counter()
        if status != 0:
            raise EngineError(error.value.decode("utf-8", errors="replace") or f"bulk error {status}")
        rows = []
        for value in output:
            row = {"id": value.band_id, "valid_count": value.valid_count,
                   "excluded_mask": value.excluded_mask, "excluded_nodata": value.excluded_nodata,
                   "excluded_nonfinite": value.excluded_nonfinite,
                   "excluded_negative": value.excluded_negative}
            for name in reducers:
                row[name] = getattr(value, name) if (value.flags & 1 or name == "sum") else None
            rows.append(row)
        metadata = {name: getattr(meta, name) for name, _ in Metadata._fields_}
        metadata.update(policy=policy, provenance="caller_asserted_transient_selection",
                        owned_copy_bytes=payload, binding_reserved_bytes=charge,
                        binding_control_bytes=charge-payload,
                        normalization="lazy_scalar_f64", persistent_identity_verified=False)
        ended = time.perf_counter()
        engine.last_call_profile = {
            "op": "bulk_reduce", "preflight_ms": (preflight_end-started)*1000,
            "snapshot_ms": (snapshot_end-preflight_end)*1000,
            "native_call_ms": (native_end-snapshot_end)*1000,
            "binding_result_ms": (ended-native_end)*1000,
            "total_ms": (ended-started)*1000, "owned_copy_bytes": payload,
            "binding_reserved_bytes": charge,
            "native_validation_ns": meta.validation_ns, "native_reduction_ns": meta.reduction_ns}
        metadata["binding_timing"] = dict(engine.last_call_profile)
        return {"bands": rows, "metadata": metadata}
    finally:
        if _native_started is not None:
            _native_started.clear()
        # Drop every private numeric backing owner before returning capacity to
        # another session. Locals also reference the most recently copied view.
        values = mask = selected = None
        owners.clear()
        band_arrays.clear()
        plan = native_windows = native_bands = req = output = error = meta = value = None
        if reserved:
            _release(reserved)
        engine._guard.release()


async def reduce_async(engine, windows, **options):
    """Off-thread snapshot and FFI. Cancellation drains before releasing owners."""
    abort, native_started = threading.Event(), threading.Event()
    pending = asyncio.create_task(asyncio.to_thread(
        reduce, engine, windows, _abort=abort, _native_started=native_started, **options))
    try:
        return await asyncio.shield(pending)
    except asyncio.CancelledError:
        abort.set()
        # Cancellation can arrive before the worker starts or while copying.
        # Only this request's native-start flag authorizes cancelling the engine;
        # a rejected busy request must not cancel another active operation.
        while not pending.done():
            if native_started.is_set():
                engine.cancel()
            try:
                await asyncio.wait_for(asyncio.shield(pending), timeout=0.01)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                continue
            except Exception:
                break
        if pending.done() and not pending.cancelled():
            pending.exception()  # consume any native cancellation diagnostic
        raise
