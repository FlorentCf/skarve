"""Release-facing policy isolation and measured index lifecycle contracts."""
import sys
from pathlib import Path

import numpy as np
import pytest
import rasterio
from rasterio.transform import from_origin

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'bindings/python'))
from raster_engine_lab import Engine, EngineError


def test_population_policy_cannot_silently_replace_planar_mode(tmp_path):
    source = tmp_path / 'population.tif'
    with rasterio.open(source, 'w', driver='GTiff', width=2, height=2, count=1,
                       dtype='float64', crs='EPSG:4326', transform=from_origin(4, 51, .5, .5)) as ds:
        ds.write(np.array([[100., -20.], [0., 10.]]), 1)
    geometry = {'type': 'Polygon', 'coordinates': [[[4, 50], [5, 50], [5, 51], [4, 51], [4, 50]]]}
    with Engine() as engine:
        engine.register_source({'location': str(source)}, id='pop')
        request = dict(op='measure_hm_population', source='pop', geometry=geometry)
        with pytest.raises(EngineError, match='explicit spherical'):
            engine.call(request)
        with pytest.raises(EngineError, match='explicit spherical'):
            engine.call(dict(request, mode='native_grid_planar'))
        spherical = engine.call(dict(request, mode='hm_straight_lonlat_spherical_v1'))
        planar = engine.measure_source(geometry, 'EPSG:4326', source='pop', statistics=['sum'])
        assert spherical['mass'] == 110
        assert planar['bands'][0]['fractional_sum'] == 90
        with pytest.raises(EngineError, match='unsupported numerical mode'):
            engine.call(dict(op='measure_source', source='pop', geometry=geometry,
                             crs='EPSG:4326', mode='hm_straight_lonlat_spherical_v1'))
        with pytest.raises(EngineError, match='unknown request field'):
            engine.call(dict(request, mode='hm_straight_lonlat_spherical_v1', statistics=['sum']))
        engine.close_source('pop')
        with pytest.raises(EngineError, match='unknown reader'):
            engine.call(dict(request, mode='hm_straight_lonlat_spherical_v1'))


def test_index_lifecycle_partition_and_source_replacement(tmp_path):
    source, index = tmp_path / 'source.tif', tmp_path / 'index'
    with rasterio.open(source, 'w', driver='GTiff', width=64, height=64, count=1,
                       dtype='int16', crs='EPSG:3857', transform=from_origin(0, 64, 1, 1),
                       tiled=True, blockxsize=16, blockysize=16) as ds:
        ds.write(np.arange(4096, dtype=np.int16).reshape(64, 64), 1)
    geometry = {'type': 'Polygon', 'coordinates': [[[.25, .5], [62.75, .5], [62.75, 63.5], [.25, 63.5], [.25, .5]]]}
    with Engine() as engine:
        engine.register_source({'location': str(source)}, id='r')
        engine.prepare_source(index, source='r', tile_edge=16, boundary_source='original')
        result = engine.measure_source(geometry, 'EPSG:3857', source='r', index=str(index), statistics=['sum'])
        phases = result['timing_ms']
        names = ['index_open_or_revalidate', 'index_header_read_validation', 'source_open_validation', 'remaining_execution_assembly']
        assert all(phases[n] >= 0 for n in names)
        assert sum(phases[n] for n in names) == pytest.approx(phases['total'], abs=1e-9)
        assert result['work']['eligible_raw_interior_tiles_avoided'] > 0
        with rasterio.open(source, 'r+') as ds:
            ds.write(np.array([[99]], dtype=np.int16), 1, window=rasterio.windows.Window(0, 0, 1, 1))
        with pytest.raises(EngineError):
            engine.measure_source(geometry, 'EPSG:3857', source='r', index=str(index), statistics=['sum'])
