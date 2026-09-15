import asyncio
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "bindings/python"))
from raster_engine_lab import Engine, EngineError


def data(n=8):
    return dict(grid=dict(width=n, height=n, transform=[0,1,0,n,0,-1], crs="LOCAL"),
                bands=[dict(values=[1.]*(n*n))])


def polygon(n):
    return dict(type="Polygon", coordinates=[[[.1,.1],[n-.1,.1],[n-.1,n-.1],[.1,n-.1],[.1,.1]]])


def test_public_binding_plan_and_errors():
    with Engine() as engine:
        engine.open_raster(data())
        engine.compile_polygon(polygon(8), "LOCAL")
        engine.prepare()
        result = engine.measure(plan="p")
        assert result["bands"][0]["fractional_sum"] == pytest.approx(7.8**2)
        with pytest.raises(EngineError):
            engine.call(dict(op="measure", source="missing"))
    with pytest.raises(EngineError, match="closed"):
        engine.call(dict(op="stats"))


def test_async_event_loop_and_cancellation():
    async def run():
        with Engine() as engine:
            engine.open_raster(data(512))
            request=dict(op="measure", source="r", geometry=polygon(512), crs="LOCAL", strategy="direct")
            pending=asyncio.create_task(engine.call_async(request))
            await asyncio.sleep(.01)
            # A running native call must not prevent the event loop waking.
            assert not pending.done()
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
            assert engine.call(dict(op="stats"))["sources"] == 1
    asyncio.run(run())
