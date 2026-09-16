#!/usr/bin/env python3
"""Installed-compatible original sample output checks; only generated local data."""
import asyncio
import json
import os
from pathlib import Path
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import unittest

import numpy as np
import rasterio
from rasterio.transform import from_origin
from skarve import Skarve, EngineError

class Windows(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
    def tearDown(self):
        self.tmp.cleanup()
    def fixture(self, dtype="float32", count=3):
        path = self.root / f"{dtype}-{count}.tif"
        data = np.arange(count * 19 * 17, dtype=dtype).reshape(count, 19, 17)
        data[:, 1, 1] = 0
        mask = np.full((19,17),255,dtype="uint8");mask[2,2] = 0
        with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
            with rasterio.open(path,"w",driver="GTiff",width=17,height=19,count=count,dtype=dtype,
                crs="EPSG:3857",transform=from_origin(0,19,1,1),nodata=0,tiled=True,blockxsize=16,blockysize=16) as ds:
                ds.write(data);ds.write_mask(mask)
                ds.scales=tuple(2. for _ in range(count));ds.offsets=tuple(3. for _ in range(count))
        return path,data,mask
    def test_all_scalar_types_and_masks(self):
        for dtype in ["uint8","int8","int16","uint16","int32","uint32","float32","float64"]:
            with self.subTest(dtype=dtype):
                path,data,mask=self.fixture(dtype)
                with Skarve() as engine, engine.infuse(path) as source:
                    out=source.read_window([1,1,7,8],[2,0])
                    for band,i in zip(out["bands"],[2,0]):
                        self.assertEqual(band["values"].tobytes(), data[i,1:9,1:8].tobytes())
                        self.assertEqual(band["mask"].tobytes(),mask[1:9,1:8].tobytes())
                        self.assertEqual(band["metadata"]["scaleBits"],"4000000000000000")
                    with self.assertRaises((EngineError,ValueError)):
                        source.read_window([0,0,17,19],[0],max_bytes=5)
                    with self.assertRaises(EngineError):
                        source.read_window([0,0,17,19],[0],working_bytes=1024)
    def test_grouped_wide_window_preserves_requested_order_and_masks(self):
        path,data,mask=self.fixture(count=40)
        selected=list(range(39,-1,-1))
        with Skarve() as engine, engine.infuse(path) as source:
            out=source.read_window([1,1,7,8],selected)
            self.assertEqual(len(out["bands"]),40)
            for band,i in zip(out["bands"],selected):
                self.assertEqual(band["values"].tobytes(),data[i,1:9,1:8].tobytes())
                self.assertEqual(band["mask"].tobytes(),mask[1:9,1:8].tobytes())

    def test_async_and_lifetime(self):
        path,data,mask=self.fixture()
        async def run():
            with Skarve() as engine, engine.infuse(path) as source:
                result=await source.read_window_async([0,0,17,19],[1])
            self.assertEqual(result["bands"][0]["values"].tobytes(),data[1].tobytes())
        asyncio.run(run())
    def test_http_identity_cancellation_and_corruption(self):
        path,data,mask=self.fixture()
        skv=self.root/"data.skv"
        with Skarve() as engine, engine.infuse(path) as source:
            source.compile(skv,chunk_edge=64,band_group=3)
        class Handler(BaseHTTPRequestHandler):
            generation='"one"'
            wait=0
            ranges=[]
            def log_message(self,*args): pass
            def do_HEAD(self):
                self.send_response(200);self.send_header("Content-Length",str(skv.stat().st_size));self.send_header("ETag",self.generation);self.send_header("Accept-Ranges","bytes");self.end_headers()
            def do_GET(self):
                import time
                if self.headers.get("If-Match") not in (None, self.generation):
                    self.send_response(412);self.end_headers();return
                start,end=map(int,self.headers["Range"].removeprefix("bytes=").split("-"))
                self.ranges.append([start,end]);time.sleep(self.wait)
                content=skv.read_bytes()[start:end+1]
                self.send_response(206);self.send_header("Content-Length",str(len(content)));self.send_header("Content-Range",f"bytes {start}-{end}/{skv.stat().st_size}");self.send_header("ETag",self.generation);self.end_headers()
                try:self.wfile.write(content)
                except (BrokenPipeError,ConnectionResetError):pass
        server=ThreadingHTTPServer(("127.0.0.1",0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        spec={"location":f"http://127.0.0.1:{server.server_port}/data.skv","http":{"allow_http":True,"max_requests":100,"max_download_bytes":4<<20,"cache_bytes":0}}
        try:
            with Skarve() as engine, engine.infuse(spec) as source:
                result=source.read_window([0,0,17,19],[2,0])
                self.assertEqual(result["bands"][0]["values"].tobytes(),data[2].tobytes())
                Handler.generation='"two"'
                with self.assertRaises(EngineError):source.read_window([0,0,17,19],[0])
            Handler.generation='"one"'
            async def abort():
                with Skarve() as engine, engine.infuse(spec) as source:
                    Handler.wait=.2
                    task=asyncio.create_task(source.read_window_async([0,0,17,19],[0]))
                    await asyncio.sleep(.02);task.cancel()
                    with self.assertRaises(asyncio.CancelledError):await task
                    Handler.wait=0
            asyncio.run(abort())
            self.assertTrue(Handler.ranges)
        finally:
            server.shutdown();thread.join();server.server_close()
        # A retained reader rejects source mutation without returning partial pixels.
        with Skarve() as engine, engine.infuse(skv) as source:
            with skv.open("r+b") as file:file.seek(-1,2);file.write(b"X")
            with self.assertRaises(EngineError):source.read_window([0,0,17,19],[0])

if __name__ == "__main__":unittest.main()
