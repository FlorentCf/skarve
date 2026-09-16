"""Wide prefix admission and source invariants, with no timing performance claim.

Pass the synthetic TIFF from source_buffer_fixture.py. Padding makes the source
larger than512KiB without changing its raster interpretation; it is not a COG
performance fixture. Only controlled loopback HTTP is accessed.
"""
import asyncio
import json
import pathlib
import shutil
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'bindings/python'))
from raster_engine_lab import Engine, EngineError

with tempfile.TemporaryDirectory() as directory:
    path = pathlib.Path(directory) / 'padded.tif'
    shutil.copyfile(sys.argv[1], path)
    with path.open('ab') as stream:
        stream.write(bytes(512 * 1024 + 4096))
    state = {'etag': '"fixed"', 'ranges': [], 'reject': False, 'pause': False}
    entered, resume = threading.Event(), threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            self.send_response(200)
            self.send_header('ETag', state['etag'])
            self.send_header('Content-Length', str(path.stat().st_size))
            self.send_header('Accept-Ranges', 'bytes')
            self.end_headers()

        def do_GET(self):
            lo, hi = map(int, self.headers['Range'][6:].split('-'))
            hi = min(hi, path.stat().st_size - 1)
            state['ranges'].append((lo, hi, self.headers.get('If-Match')))
            prefix = lo == 0 and hi == 512 * 1024 - 1
            if self.headers.get('If-Match') != state['etag'] or (prefix and state['reject']):
                self.send_response(412)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            if prefix and state['pause']:
                entered.set()
                resume.wait(5)
            self.send_response(206)
            self.send_header('ETag', state['etag'])
            self.send_header('Content-Length', str(hi - lo + 1))
            self.send_header('Content-Range', f'bytes {lo}-{hi}/{path.stat().st_size}')
            self.end_headers()
            with path.open('rb') as stream:
                stream.seek(lo)
                body = stream.read(hi - lo + 1)
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    def spec(seed, **limits):
        return {'location': f'http://127.0.0.1:{server.server_port}/padded.tif',
                'format': 'geotiff', 'http': {'allow_http': True, 'max_requests': 256,
                'max_download_bytes': 4 << 20, 'max_range_bytes': 4 << 20,
                'cache_bytes': 2 << 20, 'metadata_prefetch_bytes': seed, **limits}}

    records, outputs = [], []
    try:
        choices = [(0, 0), (16384, 0), (512 * 1024, 0)]
        if '--small-pages' in sys.argv:
            choices += [(0, 65536), (512 * 1024, 65536)]
        for seed, pages in choices:
            state['ranges'] = []
            with Engine() as engine, engine.infuse(spec(seed, small_read_page_bytes=pages)) as source:
                result = source.read_window([1, 1, 7, 8], [2, 0])
                outputs.append([(bytes(band['values']), bytes(band['mask']), band['metadata']) for band in result['bands']])
                diagnostics = source.inspect()['diagnostics']
                assert diagnostics['metadata_prefix_seed_bytes'] == seed
                assert diagnostics['remote']['cache_capacity_bytes'] == 2 << 20
                assert diagnostics['remote']['cache_peak_bytes'] <= 2 << 20
                assert all(item[2] == '"fixed"' for item in state['ranges'])
                if seed:
                    assert (0, seed - 1, '"fixed"') in state['ranges']
                if pages and not seed:
                    assert diagnostics['remote']['small_read_page_fetches'] > 0
                assert diagnostics['remote']['small_read_page_overread_bytes'] <= diagnostics['remote']['small_read_page_fetches'] * 65535
                records.append({'seed': seed, 'pages': pages, 'cache_capacity_bytes': diagnostics['remote']['cache_capacity_bytes'],
                                'page_fetches': diagnostics['remote']['small_read_page_fetches'],
                                'page_overread_bytes': diagnostics['remote']['small_read_page_overread_bytes'],
                                'accepted_bytes': diagnostics['remote']['accepted_bytes'],
                                'cache_contained_hits': diagnostics['remote']['cache_contained_hits']})
        assert all(value == outputs[0] for value in outputs)
        for limits in [{'cache_bytes': 512 * 1024}, {'max_range_bytes': 16384},
                       {'max_download_bytes': 16384}]:
            state['ranges'] = []
            with Engine() as engine:
                try:
                    engine.infuse(spec(512 * 1024, **limits))
                except EngineError:
                    pass
                else:
                    raise AssertionError('insufficient existing bound accepted')
            assert not any(hi == 512 * 1024 - 1 for lo, hi, _ in state['ranges'])
        with Engine() as engine:
            try:
                engine.infuse(spec(512 * 1024 + 1))
            except EngineError:
                pass
            else:
                raise AssertionError('prefix larger than512KiB accepted')
        state['reject'] = True
        with Engine() as engine:
            try:
                engine.infuse(spec(512 * 1024))
            except EngineError:
                pass
            else:
                raise AssertionError('conditional prefix failure accepted')
        state['reject'] = False

        async def cancelled_open():
            with Engine() as engine:
                state['pause'] = True
                task = asyncio.create_task(engine.call_async({'op': 'register_source', 'id': 'paused', 'spec': spec(512 * 1024)}))
                assert await asyncio.to_thread(entered.wait, 3)
                task.cancel()
                resume.set()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
                else:
                    raise AssertionError('cancelled prefix returned success')
                state['pause'] = False
                with engine.infuse(spec(512 * 1024)) as source:
                    assert source.inspect()['diagnostics']['metadata_prefix_seed_bytes'] == 512 * 1024

        asyncio.run(cancelled_open())
        print(json.dumps({'ok': True, 'records': records, 'limits_preserved': True,
                          'conditional_failure_rejected': True, 'cancelled_open_rejected': True,
                          'performance_fixture': False}))
    finally:
        resume.set()
        server.shutdown()
        server.server_close()
        thread.join()
