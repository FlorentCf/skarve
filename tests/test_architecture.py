"""Normal Python API persistence, version identity and cancellation regressions."""
import concurrent.futures
import os
from pathlib import Path
import sys
import threading
import time
os.environ.setdefault('GDAL_CACHEMAX','16')
import numpy as np
import pytest
import rasterio
from affine import Affine
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'bindings/python'))
from raster_engine_lab import Engine, EngineError

@pytest.fixture
def prepared(tmp_path):
    source=tmp_path/'source.tif';index=tmp_path/'index'
    yy,xx=np.mgrid[:193,:193]
    with rasterio.open(source,'w',driver='GTiff',width=193,height=193,count=3,dtype='int16',nodata=-9999,crs='EPSG:3857',transform=Affine(1,0,0,0,-1,193),tiled=True,blockxsize=64,blockysize=64) as ds:
        for b in range(3):
            data=((xx*(b+2)+yy*3)%97-35).astype('int16');data[(xx+yy+b)%19==0]=-9999;ds.write(data,b+1)
        ds.scales=(.5,2.,1.);ds.offsets=(-3.,1.,0.)
    polygon={'type':'Polygon','coordinates':[[[.25,.25],[192.75,.25],[192.75,192.75],[.25,192.75],[.25,.25]],[[65.5,23.5],[69.75,23.5],[69.75,26.75],[65.5,26.75],[65.5,23.5]]]}
    with Engine() as engine: built=engine.prepare_file(source,index,tile_edge=64,layout='band_groups_4')
    return source,index,polygon,built

def close(a,b):
    if isinstance(a,dict):
        assert set(a)==set(b)
        for k in a: close(a[k],b[k])
    elif isinstance(a,list):
        assert len(a)==len(b)
        for x,y in zip(a,b):close(x,y)
    elif isinstance(a,(float,int)) and not isinstance(a,bool):assert a==pytest.approx(b,abs=1e-8,rel=1e-10)
    else:assert a==b

def test_fresh_python_interfaces_share_stats_and_explicit_fallback(prepared):
    source,index,polygon,built=prepared
    for options in [dict(statistics=['sum','support','min','max']),dict(statistics=['histogram'],histogram_edges=[-100.,0.,10.,100.]),dict(statistics=['weighted_mean'],weight_band=2)]:
        with Engine() as engine:
            indexed=engine.measure_file(polygon,'EPSG:3857',index=index,path=source,bands=[0,1],expected_build_id=built['build_id'],**options)
            raw=engine.measure_file(polygon,'EPSG:3857',path=source,bands=[0,1],**options)
        close(indexed['bands'],raw['bands'])
        if 'sum' in options['statistics']:assert indexed['work']['summary_tiles']>0
        else:assert indexed['strategy']=='persistent_combined_raw'

def test_prepared_snapshot_and_live_source_identity_are_distinct(prepared):
    source,index,polygon,built=prepared
    with Engine() as engine:
        before=engine.measure_file(polygon,'EPSG:3857',index=index,statistics=['sum'])
        with rasterio.open(source,'r+') as ds:ds.write(np.array([[900]],dtype='int16'),1,window=rasterio.windows.Window(90,90,1,1))
        with pytest.raises(EngineError,match='stale'):engine.measure_file(polygon,'EPSG:3857',index=index,path=source,statistics=['sum'])
        after=engine.measure_file(polygon,'EPSG:3857',index=index,statistics=['sum'])
        close(after['bands'],before['bands'])
        with pytest.raises(EngineError,match='identity mismatch'):engine.measure_file(polygon,'EPSG:3857',index=index,expected_build_id='0'*64,statistics=['sum'])
        with pytest.raises(EngineError,match='already exists'):engine.prepare_file(source,index)

def test_truncated_and_corrupt_index_fail_closed(prepared):
    _,index,polygon,_=prepared
    file=index/'summary.rsi'
    with file.open('r+b') as out:out.seek(40);byte=out.read(1);out.seek(40);out.write(bytes([byte[0]^1]))
    with Engine() as engine:
        with pytest.raises(EngineError,match='checksum'):engine.measure_file(polygon,'EPSG:3857',index=index)
    file.write_bytes(b'RSTRLAB1')
    with Engine() as engine:
        with pytest.raises(EngineError,match='bounds'):engine.measure_file(polygon,'EPSG:3857',index=index)

def test_cancelled_builder_does_not_publish_a_complete_index(tmp_path):
    source=tmp_path/'large.tif';destination=tmp_path/'cancelled'
    with rasterio.open(source,'w',driver='GTiff',width=4096,height=4096,count=1,dtype='uint8',crs='EPSG:3857',transform=Affine(1,0,0,0,-1,4096),tiled=True,blockxsize=256,blockysize=256,compress='deflate') as ds:
        tile=np.ones((256,256),dtype='uint8')
        for y in range(0,4096,256):
            for x in range(0,4096,256):ds.write(tile,1,window=rasterio.windows.Window(x,y,256,256))
    engine=Engine();started=threading.Event()
    def build():
        started.set();return engine.prepare_file(source,destination,tile_edge=16)
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future=pool.submit(build);started.wait();time.sleep(.05);engine.cancel()
            with pytest.raises(EngineError,match='cancel'):future.result(timeout=20)
        assert not destination.exists()
        partials=list(tmp_path.glob('cancelled.partial-*'));assert partials
        with pytest.raises(EngineError):engine.measure_file({'type':'Polygon','coordinates':[]},'EPSG:3857',index=partials[0])
    finally:engine.close()

def test_persistent_hierarchy_and_read_ahead_preserve_leaf_partition(prepared, tmp_path):
    source,flat,polygon,_=prepared
    index=tmp_path/'hierarchy'
    from shapely.geometry import box,mapping,shape
    with Engine() as engine:
        built=engine.prepare_file(source,index,tile_edge=16,summary_backend='hierarchy')
        assert built['summary_levels']>1 and built['parent_summary_reads']>0
        geometries=[polygon,mapping(box(-1,-1,194,194)),mapping(box(16,16,176,176)),
            mapping(box(.2,.3,192.7,192.8).difference(box(64.25,31.5,70.5,40.75))),
            {'type':'Polygon','coordinates':[[[8,8],[8.000000005,8],[181,181],[180.999999995,181],[8,8]]]}]
        for geo in geometries:
            expected=engine.measure_file(geo,'EPSG:3857',path=source,statistics=['sum','support','mean','min','max'])
            for backend in ['persistent_hierarchy','persistent_flat','direct']:
                for page in [0,4096,16384,65536]:
                    actual=engine.measure_file(geo,'EPSG:3857',index=index,backend=backend,summary_page_bytes=page,statistics=['sum','support','mean','min','max'])
                    close(actual['bands'],expected['bands'])
                    assert actual['work']['application_cache_bytes']<=page
            # Fresh sessions cannot retain query-local read-ahead buffers.
            with Engine() as fresh:
                again=fresh.measure_file(geo,'EPSG:3857',index=index,summary_page_bytes=16384,statistics=['sum','support','mean','min','max'])
                close(again['bands'],expected['bands'])
        full=engine.measure_file(mapping(box(-1,-1,194,194)),'EPSG:3857',index=index,statistics=['sum'])
        assert full['work']['summary_nodes']==1
        assert full['work']['raw_tiles']==0 and full['work']['summary_tiles']==169
        ungrouped=engine.measure_file(polygon,'EPSG:3857',index=index,backend='direct',coalesce_raw=False,statistics=['sum','support'])
        grouped=engine.measure_file(polygon,'EPSG:3857',index=index,backend='direct',coalesce_raw=True,statistics=['sum','support'])
        close(grouped['bands'],ungrouped['bands'])
        assert grouped['io']['raw_bytes']==ungrouped['io']['raw_bytes']
        assert grouped['io']['raw_reads']<ungrouped['io']['raw_reads']
        with pytest.raises(EngineError,match='hierarchical index'):
            engine.measure_file(polygon,'EPSG:3857',index=flat,backend='persistent_hierarchy')
        with pytest.raises(EngineError,match='summary_page_bytes'):
            engine.measure_file(polygon,'EPSG:3857',index=index,summary_page_bytes=2**63)


def test_optional_summary_only_uses_original_boundary_source(prepared, tmp_path):
    source,_,polygon,_=prepared
    from shapely.geometry import box,mapping
    index=tmp_path/'summary-only'
    with Engine() as engine:
        built=engine.prepare_file(source,index,tile_edge=16,summary_backend='hierarchy',boundary_source='original')
        assert built['duplicate_raw_bytes']==0 and not (index/'pixels.rsr').exists()
        assert built['total_index_and_raw_bytes']==(index/'summary.rsi').stat().st_size
        for geometry in [polygon,mapping(box(-1,-1,194,194)),mapping(box(16.001,16.001,16.002,16.002))]:
            for opts in [dict(statistics=['sum','support','mean','min','max']),
                         dict(statistics=['histogram'],histogram_edges=[-100.,0.,10.,100.]),
                         dict(statistics=['weighted_mean'],weight_band=2)]:
                expected=engine.measure_file(geometry,'EPSG:3857',path=source,bands=[1,0],**opts)
                for backend in ['auto','persistent_flat','direct']:
                    actual=engine.measure_file(geometry,'EPSG:3857',index=index,path=source,backend=backend,bands=[1,0],**opts)
                    close(actual['bands'],expected['bands'])
                    assert actual['io']['raw_reads']==0
                    assert actual['work']['duplicate_raw_total_bytes']==0
        full=engine.measure_file(mapping(box(-1,-1,194,194)),'EPSG:3857',index=index,path=source,statistics=['sum'])
        assert full['source_io']['windows']==[] and full['source_io']['adapter_calls']==0
        with pytest.raises(EngineError,match='explicit path'):
            engine.measure_file(polygon,'EPSG:3857',index=index)
        with rasterio.open(source,'r+') as ds:ds.write(np.array([[777]],dtype='int16'),1,window=rasterio.windows.Window(90,90,1,1))
        with pytest.raises(EngineError,match='stale'):
            engine.measure_file(polygon,'EPSG:3857',index=index,path=source)


def test_offset_summary_schedule_preserves_certificate_and_raw_partition(prepared,tmp_path):
    source,_,polygon,_=prepared
    from shapely.geometry import mapping,box
    for mode in ['normalized','original']:
        index=tmp_path/f'ordered-{mode}'
        with Engine() as engine:
            engine.prepare_file(source,index,tile_edge=16,summary_backend='hierarchy',boundary_source=mode)
            for geometry in [polygon,mapping(box(-1,-1,194,194)),mapping(box(16.001,16.001,16.002,16.002))]:
                for backend in ['persistent_flat','persistent_hierarchy','direct']:
                    for page in [0,4096,16384,65536]:
                        base=dict(index=index,path=source,backend=backend,summary_page_bytes=page,statistics=['sum','support','mean','min','max'])
                        expected=engine.measure_file(geometry,'EPSG:3857',**base)
                        actual=engine.measure_file(geometry,'EPSG:3857',order_summaries=True,**base)
                        close(actual['bands'],expected['bands'])
                        assert actual['work']['eligible_raw_interior_tiles_avoided']==expected['work']['eligible_raw_interior_tiles_avoided']
                        assert actual['io']['raw_bytes']==expected['io']['raw_bytes']
                        assert actual['source_io']['decoded_bytes']==expected['source_io']['decoded_bytes']
                        assert actual['work']['buffer_bound_bytes']>=expected['work']['buffer_bound_bytes']
                        reads=[r['offset'] for r in actual['io']['records'] if r['source']=='index']
                        assert reads==sorted(reads)



def test_summary_scheduling_requires_an_index(prepared):
    source,_,polygon,_=prepared
    with Engine() as engine:
        with pytest.raises(EngineError,match='persistent options require index'):
            engine.measure_file(polygon,'EPSG:3857',path=source,order_summaries=True)
