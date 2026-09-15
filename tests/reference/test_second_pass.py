"""Fresh stress cases for the edge integral, accelerated validation and reducers."""
import math
import os
from pathlib import Path
import numpy as np
import pytest
from shapely.geometry import Polygon, MultiPolygon, box, mapping
from .test_oracle import raster, run_case
from benchmarks.reference import Native

@pytest.mark.parametrize('seed',range(12))
def test_complex_integral_against_cell_intersections(seed):
    rng=np.random.default_rng(820127+seed)
    angles=np.arange(512)*2*math.pi/512
    radii=rng.uniform(.5,7,512)
    p=Polygon(np.column_stack((8+radii*np.cos(angles),8+radii*np.sin(angles))))
    assert p.is_valid
    if seed%2: p=Polygon(list(p.exterior.coords)[::-1])
    with Native() as native:
        run_case(native,raster(16,4),mapping(p),prepare=True,debug=True,
                 histogram_edges=[-10,0,10,30],weight_band=3)

@pytest.mark.parametrize('epsilon',[1e-6,1e-9,1e-12])
def test_integral_keeps_tiny_support_and_holes(epsilon):
    shapes=[box(1,1,1+epsilon,14),
            Polygon([(0,0),(16,0),(16,16),(0,16)], [[(1,1),(1,15),(15,15),(15,1)]]),
            MultiPolygon([box(0,0,1,16),box(2,0,2+epsilon,16)])]
    with Native() as native:
        for p in shapes: run_case(native,raster(16,2),mapping(p),prepare=True,debug=True)

def test_thin_rotated_strip_against_independent_decimal_oracle():
    # GEOS itself loses mean accuracy here. Compare to an independently authored
    # 80-digit world-coordinate oracle, retaining the original SPEC tolerances.
    from scripts.verification_second_pass_thin import run
    library=os.environ.get('RASTER_ENGINE_LIB',str(Path(__file__).resolve().parents[2]/'target/release/libraster_engine.so'))
    report=run(library)
    for case in report['cases']:
        assert case['decimal_intersecting']==case['native_direct_intersecting']==27
        for stats in case['statistics']:
            expected=float(stats['decimal_mean'])
            for actual in stats['native'].values():
                assert abs(actual['mean']-expected)<=1e-8+1e-10*abs(expected)
                assert abs(actual['sum']-float(stats['decimal_sum']))<=1e-8+1e-10*abs(float(stats['decimal_sum']))
                assert actual['intersecting']==27

def test_nonempty_degenerate_ring_rejected():
    with Native() as native:
        native.call({'op':'open','id':'r','raster':raster()})
        for coords in [[(0,0),(1,0),(2,0),(0,0)],[(0,0),(1,1),(2,2),(0,0)],[(0,0),(2,2),(0,2),(2,0),(0,0)]]:
            result=native.raw({'op':'measure','source':'r','geometry':{'type':'Polygon','coordinates':[coords]},'crs':'LOCAL'})
            assert not result['ok'] and 'invalid' in result['error']

def test_full_prepared_blocks_and_signed_cancellation():
    data=raster(128,3)
    data['bands'][0]['values']=[(-1 if i%2 else 1)*1e10+(i%7) for i in range(128**2)]
    with Native() as native:
        run_case(native,data,mapping(box(.25,.5,127.9,127.7)),prepare=True,debug=True)
