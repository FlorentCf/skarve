"""Retained transport/header reuse must preserve original-source integrity."""
from contextlib import contextmanager
import sys
import threading
from pathlib import Path

import numpy as np
import pytest
import rasterio
from rasterio.transform import from_origin

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / 'bindings/python'), str(ROOT / 'scripts')]
from raster_engine_lab import Engine, EngineError
from range_server import RangeServer


@pytest.fixture
def data(tmp_path):
    source, index = tmp_path / 'source.tif', tmp_path / 'index'
    values = np.arange(128*128, dtype=np.int16).reshape(128, 128)-1000
    with rasterio.open(source, 'w', driver='GTiff', width=128, height=128, count=1,
                       dtype='int16', crs='EPSG:3857', transform=from_origin(0, 128, 1, 1),
                       tiled=True, blockxsize=16, blockysize=16) as ds:
        ds.write(values, 1)
    return source, index


def polygon(offset=0):
    return dict(type='Polygon', coordinates=[[[offset+.25, .5], [110.75, .5],
                                             [110.75, 120.5], [offset+.25, 120.5], [offset+.25, .5]]])


def setup(engine, source, index):
    engine.register_source({'location': str(source)}, id='r')
    return engine.prepare_source(index, source='r', tile_edge=16, boundary_source='original')


def register(engine, index, id='i', **options):
    return engine.call(dict(op='register_index', id=id, source='r', index=str(index), **options))


def measure(engine, offset=0, **options):
    return engine.measure_source(polygon(offset), 'EPSG:3857', source='r',
                                 statistics=['sum', 'support', 'mean', 'min', 'max', 'count'], **options)


def test_new_geometries_retained_header_and_explicit_lifetime(data):
    source, index = data
    with Engine() as engine:
        built = setup(engine, source, index)
        opened = register(engine, index, expected_build_id=built['build_id'])
        assert opened['retained_value_bytes'] == opened['retained_summary_page_bytes'] == 0
        for offset in [0, 19, 45, 75, 100]:
            actual = measure(engine, offset, index_handle='i')
            expected = measure(engine, offset, index=str(index))
            assert actual['bands'] == expected['bands']
            assert actual['retained_index_handle'] is True
            assert actual['io']['index_bytes'] + 65536 == expected['io']['index_bytes']
        for options in [dict(index=str(index)), dict(read_memory_bytes=1<<20), dict(expected_build_id=built['build_id'])]:
            with pytest.raises(EngineError, match='conflicting'):
                measure(engine, index_handle='i', **options)
        for i in range(1, 16):
            register(engine, index, id=f'i{i}')
        with pytest.raises(EngineError, match='count budget'):
            register(engine, index, id='overflow')
        engine.call(dict(op='close_index', id='i'))
        with pytest.raises(EngineError, match='unknown retained index'):
            measure(engine, index_handle='i')
        register(engine, index, id='replacement')


def test_replacement_invalidates_handle_and_does_not_recover(data):
    source, index = data
    with Engine() as engine:
        setup(engine, source, index)
        register(engine, index)
        measure(engine, index_handle='i')
        with (index / 'summary.rsi').open('r+b') as out:
            out.seek(80)
            byte = out.read(1)
            out.seek(80)
            out.write(bytes([byte[0] ^ 1]))
        with pytest.raises(EngineError, match='changed'):
            measure(engine, index_handle='i')
        with pytest.raises(EngineError, match='invalidated'):
            measure(engine, index_handle='i')
        with pytest.raises(EngineError, match='invalidated'):
            engine.call(dict(op='index_info', id='i'))
        engine.call(dict(op='close_index', id='i'))


@contextmanager
def served(path):
    server = RangeServer(path, 'ok')
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=.01), daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        thread.join(timeout=3)
        server.server_close()


def test_remote_pre_post_validation_and_identity_expiry(data):
    source, index = data
    with Engine() as engine:
        setup(engine, source, index)
        with served(index / 'summary.rsi') as server:
            with pytest.raises(EngineError, match='credential-bearing or signed'):
                register(engine, server.url + '?credential=test-token')
            register(engine, server.url)
            before = len(server.records)
            actual = measure(engine, index_handle='i')
            assert actual['bands'] == measure(engine)['bands']
            records = server.records[before:]
            # Two provider contacts around the query even if metadata is retained.
            assert len(records) >= 3
            assert actual['index_transport_metrics_scope'] == 'handle lifetime'
            assert 'test-token' not in str(actual)
            original_etag = server.etag
            server.etag = '"changed"'
            with pytest.raises(EngineError):
                engine.measure_source(dict(type='Polygon', coordinates=[]), 'EPSG:3857', source='r', index_handle='i')
            server.etag = original_etag
            with pytest.raises(EngineError, match='invalidated'):
                measure(engine, index_handle='i')
            engine.call(dict(op='close_index', id='i'))
            register(engine, server.url)
            assert measure(engine, index_handle='i')['bands'] == actual['bands']
