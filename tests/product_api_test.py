"""Product aliases exercise owned sources and real paged execution."""
import asyncio
import sys
import threading
from pathlib import Path

import numpy as np
import pytest
import rasterio
from rasterio.transform import from_origin

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "bindings/python"))
from skarve import Engine, EngineError, Skarve


ZONE = {"type": "Polygon", "coordinates": [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]]}
METRICS = ["sum", "support", "mean", "min", "max"]


@pytest.fixture
def source_path(tmp_path):
    path = tmp_path / "source.tif"
    values = np.stack([np.arange(16).reshape(4, 4), np.full((4, 4), 3)]).astype("float64")
    with rasterio.open(path, "w", driver="GTiff", width=4, height=4, count=2,
                       dtype="float64", crs="EPSG:3857", transform=from_origin(0, 4, 1, 1)) as dataset:
        dataset.write(values)
    return path


def test_branded_source_mapping_defaults_aliases_and_handles(source_path, tmp_path):
    assert Skarve is Engine
    with Skarve() as sk:
        with sk.infuse({"location": str(source_path), "bands": [1, 0]}) as source:
            with sk.infuse(source_path) as other:
                assert source.id != other.id
                branded = source.carve(zone=ZONE, bands=[0], metrics=METRICS)
                explicit = source.measure(ZONE, "EPSG:3857", bands=[0], statistics=METRICS)
                assert branded["bands"] == explicit["bands"]
                assert branded["bands"][0]["fractional_sum"] == 48
                assert other.carve(zone=ZONE, bands=[0], metrics=METRICS)["bands"][0]["fractional_sum"] == 120
                with pytest.raises(EngineError, match="CRS"):
                    source.carve(zone=ZONE, crs="EPSG:4326")
                with pytest.raises(TypeError, match="not both"):
                    source.carve(zone=ZONE, metrics=METRICS, statistics=METRICS)
            built = source.ward(tmp_path / "index", boundary_source="original", tile_edge=16)
            assert built["duplicate_raw_bytes"] == 0
        with pytest.raises(EngineError, match="closed"):
            source.carve(zone=ZONE)


def test_cleave_uses_shared_job_preserves_page_order_and_releases(source_path):
    zones = [{"id": name, "version": "v1", "geometry": ZONE} for name in ("a", "b")]
    with Skarve() as sk:
        job = {"zones": zones, "slices": [
            {"id": name, "spec": {"location": str(source_path)}, "bands": [1, 0]}
            for name in ("first", "second")], "crs": "EPSG:3857", "metrics": METRICS}
        pages = list(sk.cleave(job, max_rows=1))
        assert "metrics" in job and "options" not in job
        assert all(len(page["rows"]) == 1 for page in pages)
        assert pages[-1]["complete"]
        assert {(row["zone_id"], row["slice_id"]) for page in pages for row in page["rows"]} == {
            (zone, slice_) for zone in ("a", "b") for slice_ in ("first", "second")}
        assert pages[-1]["metrics"]["geometry_cache_hits"] > 0
        with pytest.raises(EngineError, match="unknown job"):
            sk.call({"op": "job_info", "id": "batch"})
        iterator = sk.cleave(job, id="early", max_rows=1)
        next(iterator)
        iterator.close()
        with pytest.raises(EngineError, match="unknown job"):
            sk.call({"op": "job_info", "id": "early"})


def test_async_carve_has_same_complete_result(source_path):
    async def run():
        with Skarve() as sk:
            with sk.infuse(source_path) as source:
                result = await source.carve_async(zone=ZONE, bands=[0], metrics=METRICS)
                assert result["bands"][0]["fractional_sum"] == 120
    asyncio.run(run())


def test_repeated_async_cancellation_drains_before_returning():
    """A delayed FFI return models an upstream phase without an interrupt point."""
    async def run():
        with Skarve() as sk:
            entered, release = threading.Event(), threading.Event()
            original = sk._lib.re_call

            def delayed(handle, request):
                entered.set()
                assert release.wait(5), "test failed to release native return"
                return original(handle, request)

            sk._lib.re_call = delayed
            pending = asyncio.create_task(sk.call_async({"op": "stats"}))
            try:
                while not entered.is_set():
                    await asyncio.sleep(.001)
                pending.cancel()
                await asyncio.sleep(.01)
                pending.cancel()
                await asyncio.sleep(.01)
                assert not pending.done(), "cancelled awaiter released its active native request"
                with pytest.raises(EngineError, match="active"):
                    sk.close()
            finally:
                release.set()
            with pytest.raises(asyncio.CancelledError):
                await pending
            assert sk.call({"op": "stats"})["sources"] == 0
    asyncio.run(run())


def test_cancelled_unadmitted_async_request_does_not_cancel_active_owner():
    async def run():
        with Skarve() as sk:
            entered, release = threading.Event(), threading.Event()
            original, original_cancel = sk._lib.re_call, sk._lib.re_cancel
            cancelled = []

            def delayed(handle, request):
                entered.set()
                assert release.wait(5)
                return original(handle, request)

            def cancel(handle):
                cancelled.append(handle)
                original_cancel(handle)

            sk._lib.re_call, sk._lib.re_cancel = delayed, cancel
            owner = asyncio.create_task(sk.call_async({"op": "stats"}))
            try:
                while not entered.is_set():
                    await asyncio.sleep(.001)
                rejected = asyncio.create_task(sk.call_async({"op": "stats"}))
                await asyncio.sleep(0)
                rejected.cancel()
                with pytest.raises((asyncio.CancelledError, EngineError)):
                    await rejected
                assert not cancelled, "an unadmitted request cancelled another call"
            finally:
                release.set()
            assert (await owner)["sources"] == 0
    asyncio.run(run())
