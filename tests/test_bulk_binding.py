"""Installed-compatible public Python binary workflow and bounded ownership."""
import asyncio
import ctypes
import sys
import threading
import time
from pathlib import Path

import numpy as np
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "bindings/python"))
from raster_engine_lab import Engine, EngineError
import skarve_bulk as bulk


def windows(values, bands=1, **band_options):
    return [{"bands": [{"id": i+17, "values": values, **band_options} for i in range(bands)]}]


def ordered(values):
    result = 0.0
    for value in values:
        value = float(value)
        if np.isfinite(value) and value >= 0:
            result += value
    return result


@pytest.mark.parametrize("bands", [1, 18, 20, 36, 40, 64])
@pytest.mark.parametrize("dtype", [np.float32, np.float64])
def test_band_counts_and_original_dtype(bands, dtype):
    values = np.array([1.25, 0, 2.5, 3.75], dtype=dtype)
    with Engine() as engine:
        result = engine.bulk_reduce(windows(values, bands), policy="hm_demographics_ordered_v1",
                                    reducers=("sum", "min", "max", "mean"))
    assert len(result["bands"]) == bands
    for i, row in enumerate(result["bands"]):
        assert row == {"id": i+17, "sum": 7.5, "min": 0, "max": 3.75, "mean": 1.875,
                       "valid_count": 4, "excluded_mask": 0, "excluded_nodata": 0,
                       "excluded_nonfinite": 0, "excluded_negative": 0}
    meta = result["metadata"]
    assert meta["owned_copy_bytes"] == values.nbytes*bands
    assert meta["payload_bytes"] == values.nbytes*bands
    assert meta["persistent_identity_verified"] is False
    assert meta["native_owned_bytes"] < 8192
    assert bulk.reserved_snapshot_bytes() == 0


def test_threshold_and_window_partial_order_are_not_compensated():
    values = np.array([2**24, .5-2**-25, *([2**-30]*32)], dtype=np.float32)
    with Engine() as engine:
        strict = engine.bulk_reduce(windows(values))
        hm = engine.bulk_reduce(windows(values), policy="hm_demographics_ordered_v1")
        parts = windows(np.array([1e16], dtype=np.float64)) + windows(np.array([1, 1], dtype=np.float64))
        split = engine.bulk_reduce(parts, policy="hm_demographics_ordered_v1")
    assert strict["bands"][0]["sum"] == 16777216.5
    assert hm["bands"][0]["sum"] == ordered(values) == 16777216.49999997
    assert split["bands"][0]["sum"] == 10000000000000002.0


def test_contiguous_offset_views_are_copied_without_parent_payload():
    backing = np.arange(200, dtype=np.float32)
    view = backing[51:67]
    with Engine() as engine:
        result = engine.bulk_reduce(windows(view), policy="hm_demographics_ordered_v1")
    assert result["bands"][0]["sum"] == ordered(view)
    assert result["metadata"]["owned_copy_bytes"] == view.nbytes
    assert backing[50] == 50
    assert backing[67] == 67


def test_byte_bit_masks_nodata_and_invalid_nan():
    values = np.array([1, np.nan, -2, -99, 0, 3, np.inf], dtype=np.float32)
    bytes_mask = np.array([0, 1, 0, 1, 1, 1, 1, 1], dtype=np.uint8)
    bits_mask = np.array([0b11111010], dtype=np.uint8)
    with Engine() as engine:
        for mask, kind in [(bytes_mask, "bytes"), (bits_mask, "bits")]:
            result = engine.bulk_reduce(windows(values, validity=mask, validity_kind=kind,
                validity_offset=1, nodata=-99), policy="hm_demographics_ordered_v1")
            row = result["bands"][0]
            assert row["sum"] == 4
            assert row["valid_count"] == 3
            assert row["excluded_mask"] == 1
            assert row["excluded_nodata"] == 1
            assert row["excluded_negative"] == 1
            assert row["excluded_nonfinite"] == 1


@pytest.mark.parametrize("kind", ["u32", "u64", "spans"])
def test_sparse_repeated_selection_and_empty_output(kind):
    values = np.array([1, 2, 3, 4], dtype=np.float64)
    selection = ({"spans": np.array([[3, 1], [0, 1], [3, 1]], dtype=np.uint64)}
                 if kind == "spans" else np.array([3, 0, 3], dtype=np.uint32 if kind == "u32" else np.uint64))
    win = windows(values); win[0]["selection"] = selection
    with Engine() as engine:
        result = engine.bulk_reduce(win)
        empty = windows(values); empty[0]["selection"] = np.array([], dtype=np.uint32)
        none = engine.bulk_reduce(empty, reducers=("sum", "mean", "min", "max"))
    assert result["bands"][0]["sum"] == 9
    assert result["bands"][0]["valid_count"] == 3
    assert {key: none["bands"][0][key] for key in ("sum", "min", "max", "mean")} == {
        "sum": 0, "min": None, "max": None, "mean": None}


def test_noncontiguous_and_non_native_arrays_are_explicitly_rejected():
    with Engine() as engine:
        for values in [np.arange(20, dtype=np.float32)[::2], np.ones(3, dtype=">f4"),
                       np.ones(3, dtype=np.int32), [1, 2, 3]]:
            with pytest.raises(EngineError):
                engine.bulk_reduce(windows(values))
            assert bulk.reserved_snapshot_bytes() == 0


def test_malformed_control_budgets_and_native_errors_release_reservation():
    values = np.arange(3, dtype=np.float32)
    with Engine() as engine:
        for options in [{"max_payload_bytes": 4}, {"max_contributions": 1},
                        {"policy": "unknown"}, {"reducers": ("sum", "sum")},
                        {"reducers": ("quantile",)}, {"max_payload_bytes": bulk.MAX_BYTES+1}]:
            with pytest.raises(EngineError):
                engine.bulk_reduce(windows(values), **options)
            assert bulk.reserved_snapshot_bytes() == 0
        wrong = windows(values); wrong[0]["selection"] = np.array([9], dtype=np.uint32)
        with pytest.raises(EngineError, match="index"):
            engine.bulk_reduce(wrong)
        with pytest.raises(EngineError, match="nonfinite"):
            engine.bulk_reduce(windows(np.array([np.nan], dtype=np.float64)))
        mismatch = windows(values) + windows(values)
        mismatch[1]["bands"][0]["id"] += 1
        with pytest.raises(EngineError, match="identities"):
            engine.bulk_reduce(mismatch)
    assert bulk.reserved_snapshot_bytes() == 0


def test_process_budget_reserves_before_copy(monkeypatch):
    with Engine() as engine:
        monkeypatch.setattr(bulk, "MAX_SNAPSHOTS", 100)
        with pytest.raises(EngineError, match="process bulk snapshot"):
            engine.bulk_reduce(windows(np.ones(30, dtype=np.float32)))
    assert bulk.reserved_snapshot_bytes() == 0


def test_busy_and_close_share_ordinary_engine_lifecycle():
    engine = Engine()
    engine._guard.acquire()
    try:
        with pytest.raises(EngineError, match="busy"):
            engine.bulk_reduce(windows(np.ones(3, dtype=np.float32)))
        with pytest.raises(EngineError, match="active"):
            engine.close()
    finally:
        engine._guard.release()
    engine.close()
    with pytest.raises(EngineError, match="closed"):
        engine.bulk_reduce(windows(np.ones(3, dtype=np.float32)))


def test_snapshot_ownership_survives_caller_mutation_before_native(monkeypatch):
    values = np.arange(32, dtype=np.float32)
    original = values.copy()
    with Engine() as engine:
        native = engine._lib.re_bulk
        class Wrapped:
            def __call__(self, *args):
                values.fill(1000)
                return native(*args)
        # Set original signature because the wrapper stores but cannot propagate it.
        native.argtypes = [ctypes.c_void_p, ctypes.POINTER(bulk.Request), ctypes.POINTER(bulk.Result),
                           ctypes.c_uint64, ctypes.POINTER(bulk.Metadata), ctypes.c_void_p, ctypes.c_uint64]
        native.restype = ctypes.c_int32
        monkeypatch.setattr(engine._lib, "re_bulk", Wrapped())
        result = engine.bulk_reduce(windows(values))
    assert result["bands"][0]["sum"] == ordered(original)
    assert bulk.reserved_snapshot_bytes() == 0


def test_async_cancel_during_snapshot_drains_and_allows_reuse(monkeypatch):
    snapshot_entered = threading.Event()
    copy = bulk._copy_array
    def slowed(*args):
        snapshot_entered.set()
        time.sleep(.04)
        return copy(*args)
    monkeypatch.setattr(bulk, "_copy_array", slowed)

    async def run():
        with Engine() as engine:
            task = asyncio.create_task(engine.bulk_reduce_async(windows(np.ones(100, dtype=np.float32))))
            while not snapshot_entered.is_set():
                await asyncio.sleep(.001)
            assert not task.done()
            with pytest.raises(EngineError, match="active"):
                engine.close()
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert bulk.reserved_snapshot_bytes() == 0
            assert engine.bulk_reduce(windows(np.ones(2, dtype=np.float32)))["bands"][0]["sum"] == 2
    asyncio.run(run())


def test_async_native_cancel_keeps_event_loop_live_and_handle_safe():
    async def run():
        with Engine() as engine:
            values = np.ones(500_000, dtype=np.float32)
            task = asyncio.create_task(engine.bulk_reduce_async(windows(values, 40),
                policy="hm_demographics_ordered_v1"))
            await asyncio.sleep(.001)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert bulk.reserved_snapshot_bytes() == 0
            assert engine.bulk_reduce(windows(np.ones(2, dtype=np.float32)))["bands"][0]["sum"] == 2
    asyncio.run(run())


def test_async_busy_does_not_cancel_another_request(monkeypatch):
    calls = []
    async def run():
        with Engine() as engine:
            monkeypatch.setattr(engine, "cancel", lambda: calls.append("cancel"))
            engine._guard.acquire()
            try:
                with pytest.raises(EngineError, match="busy"):
                    await engine.bulk_reduce_async(windows(np.ones(1, dtype=np.float32)))
            finally:
                engine._guard.release()
    asyncio.run(run())
    assert calls == []

