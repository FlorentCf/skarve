"""Real NetCDF band-group reader reuse through the public native job API."""
import copy
import sys
from pathlib import Path
from scipy.io import netcdf_file
import numpy as np
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'bindings/python'))
from raster_engine_lab import Engine


def fixture(path):
    with netcdf_file(path,'w',version=1) as dataset:
        dataset.createDimension('time',4);dataset.createDimension('lat',2);dataset.createDimension('lon',2)
        t=dataset.createVariable('time','f8',('time',));t.units='hours since 2000-01-01';t[:]=[0,1,2,3]
        lat=dataset.createVariable('lat','f8',('lat',));lat.units='degrees_north';lat.standard_name='latitude';lat[:]=[1,0]
        lon=dataset.createVariable('lon','f8',('lon',));lon.units='degrees_east';lon.standard_name='longitude';lon[:]=[0,1]
        v=dataset.createVariable('air','f8',('time','lat','lon'));v.units='K'
        v[:]=np.broadcast_to(np.arange(1.,5.)[:,None,None],(4,2,2))


def job(path):
    original={'location':str(path),'format':'netcdf','variable':'air','bands':[0,1,2,3],'crs':'EPSG:4326'}
    reversed_spec=dict(original,bands=[3,2,1,0])
    slices=[{'id':str(i),'spec':copy.deepcopy(original),'bands':[i]} for i in range(4)]
    slices += [{'id':'other','spec':reversed_spec,'bands':[0]},{'id':'revisit','spec':original,'bands':[0]}]
    geometry={'type':'Polygon','coordinates':[[[-.5,-.5],[1.5,-.5],[1.5,1.5],[-.5,1.5],[-.5,-.5]]]}
    return {'zones':[{'id':'z','version':'1','geometry':geometry}], 'slices':slices,
            'crs':'EPSG:4326','tile_edge':32,'options':{'statistics':['sum','support','mean']}}


def test_actual_netcdf_group_reuses_one_reader_and_resumes_periodic_checkpoint(tmp_path):
    path=tmp_path/'climate.nc';fixture(path);definition=job(path)
    with Engine() as engine:
        pages=list(engine.batch_pages(definition,max_rows=1,checkpoint_interval=2))
        assert pages[3]['metrics']['source_opens']==1
        assert pages[3]['metrics']['source_acquisitions']==4
        assert pages[3]['metrics']['lazy_source_cache_hits']==3
        assert pages[-1]['metrics']['source_opens']==3
        assert pages[-1]['metrics']['source_acquisitions']==6
        assert pages[-1]['metrics']['windows_read']==6
        assert ['checkpoint' in p for p in pages]==[False,True,False,True,False,True]
        rows=[r for p in pages for r in p['rows']]
        assert [r['bands'][0]['fractional_sum'] for r in rows]==[4,8,12,16,16,4]
        resumed=list(engine.batch_pages(definition,checkpoint=pages[1]['checkpoint'],max_rows=1,checkpoint_interval=2))
        assert [r for p in resumed for r in p['rows']]==rows[2:]
        assert resumed[-1]['metrics']['source_opens']==3


def test_full_transport_configuration_equality_controls_reader_reuse(tmp_path):
    path=tmp_path/'climate.nc';fixture(path);definition=job(path)
    definition['slices']=definition['slices'][:2]
    definition['slices'][1]['spec']['http']={'headers':{'X-Fixture':'changed'},'max_requests':2}
    with Engine() as engine:
        pages=list(engine.batch_pages(definition,max_rows=1))
        assert pages[-1]['metrics']['source_opens']==2
        assert pages[-1]['metrics']['lazy_source_cache_hits']==0
        assert pages[0]['rows'][0]['source_id']==pages[1]['rows'][0]['source_id']
        assert [p['rows'][0]['bands'][0]['fractional_sum'] for p in pages]==[4,8]


def test_cached_http_reader_keeps_exact_request_deltas_and_rechecks_version(tmp_path):
    import threading
    import pytest
    import rasterio
    from affine import Affine
    from raster_engine_lab import EngineError
    sys.path.insert(0,str(ROOT/'scripts'))
    from range_server import RangeServer
    path=tmp_path/'original.tif'
    with rasterio.open(path,'w',driver='GTiff',width=16,height=16,count=1,dtype='float64',
                       crs='EPSG:3857',transform=Affine(1,0,0,0,-1,16),compress='deflate') as dataset:
        dataset.write(np.arange(256,dtype=float).reshape(1,16,16))
    server=RangeServer(path)
    thread=threading.Thread(target=lambda:server.serve_forever(poll_interval=.01),daemon=True);thread.start()
    try:
        spec={'location':server.url,'http':{'allow_http':True,'cache_bytes':256<<10,'max_requests':64,
              'max_range_bytes':16<<10,'max_download_bytes':1<<20}}
        geometry={'type':'Polygon','coordinates':[[[0,0],[16,0],[16,16],[0,16],[0,0]]]}
        definition={'zones':[{'id':'z','version':'1','geometry':geometry}],
                    'slices':[{'id':str(i),'spec':copy.deepcopy(spec)} for i in range(4)],
                    'crs':'EPSG:3857','tile_edge':32,'options':{'statistics':['sum','support']}}
        with Engine() as engine:
            engine.call({'op':'start_job','id':'remote','job':definition})
            first=engine.call({'op':'next_job','id':'remote','max_rows':1})
            second=engine.call({'op':'next_job','id':'remote','max_rows':1})
            metrics=second['metrics']
            assert metrics['source_opens']==1 and metrics['source_acquisitions']==2 and metrics['lazy_source_cache_hits']==1
            assert second['rows'][0]['bands']==first['rows'][0]['bands']
            assert metrics['network_requests']==len(server.records)
            assert metrics['network_get_requests']==sum(r['method']=='GET' for r in server.records)
            assert metrics['network_head_requests']==sum(r['method']=='HEAD' for r in server.records)
            assert metrics['network_body_bytes']==sum(r['sent_bytes'] for r in server.records)
            assert metrics['transfer_cache_hits']>0
            # The compressed cache is warm, but a new slice must revalidate it.
            server.etag='"replacement-version"'
            with pytest.raises(EngineError,match='changed|identity|version|validator'):
                engine.call({'op':'next_job','id':'remote','max_rows':1})
            assert engine.call({'op':'job_info','id':'remote'})['next_row']==2
            engine.call({'op':'close_job','id':'remote'})
    finally:
        server.shutdown();thread.join(timeout=3);server.server_close()
