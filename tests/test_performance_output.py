"""Output modes preserve the numeric contract and checkpoint ordering."""
import copy,json,sys
from pathlib import Path
import pytest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'bindings/python'))
from raster_engine_lab import Engine,EngineError
FIELDS=['band','fractional_sum','covered_cell_equivalents','valid_cell_count','coverage_weighted_mean','min','max']
def setup(e):
    e.open_raster({'grid':{'width':9,'height':7,'transform':[0,1,0,7,0,-1],'crs':'LOCAL'},'bands':[{'values':[None if i%11==0 else i-20 for i in range(63)],'unit':'u\n🗺️'}]},id='r')
    zones=[{'id':f'z{i}\n\"🗺️','version':'v\t1','geometry':{'type':'Polygon','coordinates':[[[i/8,.125],[7.5,.125],[7.5,6.5],[i/8,6.5],[i/8,.125]]]}} for i in range(8)]
    return {'zones':zones,'slices':[{'id':f'd{i}','source':'r','time':str(i),'bands':[0]} for i in range(3)],'crs':'LOCAL','options':{'statistics':['sum','support','mean','min','max','count']},'geometry_layout':'auto'}
def normalize(rows):return [(r['result_id'],r['zone_id'],r['zone_version'],r['slice_id'],[{k:b[k] for k in FIELDS} for b in r['bands']]) for r in rows]
def test_numeric_descriptor_strict_values_and_partial_slice_resume():
    with Engine() as e:
        job=setup(e);full=list(e.batch(job,max_rows=3));numeric=dict(job,output_mode='numeric')
        pages=list(e.batch_pages(numeric,max_rows=3));rows=[r for p in pages for r in p['rows']]
        assert normalize(full)==normalize(rows)
        for p in pages:
            d=p['descriptor'];assert d['schema']=='skarve_numeric_six_v1' and d['bands'][0]['unit']=='u\n🗺️'
            assert all(r['slice_id']==d['slice_id'] for r in p['rows'])
        checkpoint=pages[0]['checkpoint']
        resumed=list(e.batch(numeric,max_rows=2,checkpoint=checkpoint))
        assert normalize(resumed)==normalize(rows[3:])
        p=e.last_call_profile
        assert p['native']['serialization_ms']>=0 and p['binding_parse_ms']>=0
        too_small=copy.deepcopy(numeric);too_small['budget']={'output_bytes':128}
        with pytest.raises(EngineError,match='output slice exceeds'):list(e.batch(too_small))
        assert normalize(list(e.batch(numeric,max_rows=1)))==normalize(rows)
def test_numeric_rejects_unrepresented_reducer_contract():
    with Engine() as e:
        job=setup(e);job['output_mode']='numeric';job['options']['statistics'].append('variance')
        with pytest.raises(EngineError,match='numeric output requires'):list(e.batch(job))

def test_numeric_masks_multiband_empty_and_cross_mode_resume():
    with Engine() as e:
        job=setup(e)
        e.open_raster({'grid':{'width':9,'height':7,'transform':[0,1,0,7,0,-1],'crs':'LOCAL'},'bands':[{'values':[None]*63},{'values':[i-31 for i in range(63)]}]},id='m')
        for sl in job['slices']:sl.update(source='m',bands=[1,0])
        job['zones'].append({'id':'outside','version':'1','geometry':{'type':'Polygon','coordinates':[[[20,20],[21,20],[21,21],[20,21],[20,20]]]}})
        full_pages=list(e.batch_pages(job,max_rows=2));full=[r for p in full_pages for r in p['rows']]
        numeric=dict(job,output_mode='numeric')
        pages=list(e.batch_pages(numeric,max_rows=2));lean=[r for p in pages for r in p['rows']]
        assert normalize(full)==normalize(lean)
        assert all(b['min'] is None and b['coverage_weighted_mean'] is None for row in lean for b in row['bands'] if b['band']==0)
        assert normalize(list(e.batch(numeric,checkpoint=full_pages[0]['checkpoint'])))==normalize(full[2:])
        assert normalize(list(e.batch(job,checkpoint=pages[0]['checkpoint'])))==normalize(full[2:])
        job['expression']={'op':'band','band':1};job['mask']={'op':'greater','left':{'op':'band','band':1},'right':{'op':'constant','value':2}}
        for sl in job['slices']:sl['bands']=[0]
        expected=list(e.batch(job));actual=list(e.batch(dict(job,output_mode='numeric')))
        assert normalize(expected)==normalize(actual)


def test_checkpoint_storage_reserved_and_oversized_pin_rejected():
    with Engine() as e:
        job=setup(e)
        pages=list(e.batch_pages(job,max_rows=3))
        assert pages[-1]['metrics']['checkpoint_reserved_bytes']==len(job['slices'])*(32<<10)
        checkpoint=copy.deepcopy(pages[0]['checkpoint'])
        checkpoint['pins'][0]['source_id']='x'*1025
        with pytest.raises(EngineError,match='checkpoint pin identifier'):
            list(e.batch(job,checkpoint=checkpoint))
        checkpoint=copy.deepcopy(pages[0]['checkpoint'])
        checkpoint['pins'][0]['grid_id']='x'*65
        with pytest.raises(EngineError,match='checkpoint pin identifier'):
            list(e.batch(job,checkpoint=checkpoint))
