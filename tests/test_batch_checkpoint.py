"""Periodic checkpoint emission preserves ordering, resume, and source work."""
import sys
from pathlib import Path
import pytest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'bindings/python'))
from raster_engine_lab import Engine,EngineError


def setup(engine):
    raster={'grid':{'width':4,'height':2,'transform':[0,1,0,2,0,-1],'crs':'LOCAL'},
            'bands':[{'values':list(range(8))}]}
    engine.open_raster(raster,id='r')
    geometry={'type':'Polygon','coordinates':[[[0,0],[4,0],[4,2],[0,2],[0,0]]]}
    return {'zones':[{'id':str(i),'version':'1','geometry':geometry} for i in range(3)],
            'slices':[{'id':str(i),'source':'r'} for i in range(3)],'crs':'LOCAL',
            'tile_edge':32,'options':{'statistics':['sum','support','mean']}}


def test_periodic_pages_resume_and_default_compatibility():
    with Engine() as engine:
        job=setup(engine)
        default=list(engine.batch_pages(job,max_rows=1))
        periodic=list(engine.batch_pages(job,max_rows=1,checkpoint_interval=4))
        assert all('checkpoint' in p for p in default)
        assert ['checkpoint' in p for p in periodic]==[False,False,False,True,False,False,False,True,True]
        assert [r for p in periodic for r in p['rows']]==[r for p in default for r in p['rows']]
        assert periodic[-1]['metrics']['windows_read']==default[-1]['metrics']['windows_read']==3
        checkpoint=periodic[3]['checkpoint']
        assert checkpoint['next_row']==4
        resumed=list(engine.batch(job,checkpoint=checkpoint,max_rows=1,checkpoint_interval=4))
        assert resumed==[r for p in default[4:] for r in p['rows']]
        assert periodic[-1]['checkpoint']['next_row']==9


def test_info_checkpoint_does_not_advance_or_read_and_false_still_finalizes():
    with Engine() as engine:
        job=setup(engine)
        engine.call({'op':'start_job','id':'low','job':job})
        first=engine.call({'op':'next_job','id':'low','max_rows':1,'include_checkpoint':False})
        assert 'checkpoint' not in first
        info=engine.call({'op':'job_info','id':'low'})
        assert info['next_row']==info['checkpoint']['next_row']==1
        assert info==engine.call({'op':'job_info','id':'low'})
        second=engine.call({'op':'next_job','id':'low','max_rows':1,'include_checkpoint':False})
        assert second['metrics']['windows_read']==first['metrics']['windows_read']==1
        assert second['metrics']['source_acquisitions']==first['metrics']['source_acquisitions']==1
        rows=first['rows']+second['rows']
        while not second['complete']:
            second=engine.call({'op':'next_job','id':'low','max_rows':1,'include_checkpoint':False})
            rows.extend(second['rows'])
        assert 'checkpoint' in second and second['checkpoint']['next_row']==9
        complete_again=engine.call({'op':'next_job','id':'low','include_checkpoint':False})
        assert complete_again['complete'] and complete_again['rows']==[] and 'checkpoint' in complete_again
        assert complete_again['metrics']==second['metrics']
        engine.call({'op':'close_job','id':'low'})
        resumed=list(engine.batch(job,checkpoint=info['checkpoint'],checkpoint_interval=100))
        assert first['rows']+resumed==rows


@pytest.mark.parametrize('interval',[0,-1,1.5,True,None])
def test_invalid_interval_fails_before_job_creation(interval):
    with Engine() as engine:
        job=setup(engine)
        with pytest.raises(ValueError,match='positive integer'):
            list(engine.batch_pages(job,id='invalid',checkpoint_interval=interval))
        with pytest.raises(EngineError,match='unknown job'):
            engine.call({'op':'job_info','id':'invalid'})


def test_native_checkpoint_flag_is_a_strict_boolean_and_error_keeps_cursor():
    with Engine() as engine:
        job=setup(engine)
        engine.call({'op':'start_job','id':'low','job':job})
        for bad in [None,0,1,'false']:
            with pytest.raises(EngineError):
                engine.call({'op':'next_job','id':'low','include_checkpoint':bad})
            assert engine.call({'op':'job_info','id':'low'})['next_row']==0
        assert 'checkpoint' in engine.call({'op':'next_job','id':'low','max_rows':1})
        engine.call({'op':'close_job','id':'low'})


def test_unvisited_lazy_source_declarations_are_resume_identity_without_opening():
    import copy
    with Engine() as engine:
        job=setup(engine)
        job['zones']=job['zones'][:1]
        future={'location':'/definitely-missing/future-original.nc','format':'netcdf',
                'variable':'air','crs':'LOCAL','longitude_shift':0,'bands':[0],
                'identity':{'sha256':'ab'*32,'byte_length':123,'policy':'verify','etag':'first'},
                'http':{'headers':{'X-Fixture':'old'},'header_env':{'X-Other':'MISSING_TEST_ENV'}}}
        job['slices']=[{'id':'first','source':'r'},{'id':'future','spec':future}]
        engine.call({'op':'start_job','id':'original','job':job})
        first=engine.call({'op':'next_job','id':'original','max_rows':1})
        checkpoint=first['checkpoint']
        assert checkpoint['next_row']==1 and checkpoint['pins'][0] and checkpoint['pins'][1] is None
        engine.call({'op':'close_job','id':'original'})
        for field,value in [('format','geotiff'),('variable','other'),('crs','OTHER'),
                            ('longitude_shift',-360),('bands',[1]),
                            ('identity',dict(future['identity'],sha256='cd'*32)),
                            ('identity',dict(future['identity'],byte_length=124)),('identity',None)]:
            changed=copy.deepcopy(job)
            changed['slices'][1]['spec'][field]=value
            # start_job rejects the fingerprint, before any next_job source IO.
            with pytest.raises(EngineError,match='checkpoint incompatible'):
                engine.call({'op':'start_job','id':'changed','job':changed,'checkpoint':checkpoint})
            with pytest.raises(EngineError,match='unknown job'):
                engine.call({'op':'job_info','id':'changed'})
        moved=copy.deepcopy(job)
        moved['slices'][1]['spec'].update(location='/another-missing/path.nc',
            http={'headers':{'X-Fixture':'new'},'header_env':{'X-Other':'ANOTHER_MISSING_ENV'},'max_requests':2})
        moved['slices'][1]['spec']['identity'].update(policy='trusted_manifest',etag='second',sha256=('ab'*32).upper())
        info=engine.call({'op':'start_job','id':'moved','job':moved,'checkpoint':checkpoint})
        assert info['fingerprint']==checkpoint['fingerprint'] and info['next_row']==1
        assert engine.call({'op':'job_info','id':'moved'})==info
        engine.call({'op':'close_job','id':'moved'})
        # Transport changes do not bypass verification of already committed pins.
        engine.call({'op':'close', 'source':'r'})
        engine.open_raster({'grid':{'width':4,'height':2,'transform':[0,1,0,2,0,-1],'crs':'LOCAL'},
                            'bands':[{'values':[9]*8}]},id='r')
        engine.call({'op':'start_job','id':'stale','job':moved,'checkpoint':checkpoint})
        with pytest.raises(EngineError,match='source version changed'):
            engine.call({'op':'next_job','id':'stale'})
        assert engine.call({'op':'job_info','id':'stale'})['next_row']==1
        engine.call({'op':'close_job','id':'stale'})
