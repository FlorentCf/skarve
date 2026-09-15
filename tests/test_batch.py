"""Independent overlap oracle, real shared-window counts, and batch lifecycle."""
import copy
import sys
from pathlib import Path
from fractions import Fraction
import numpy as np
import pytest
import rasterio
from affine import Affine
from shapely.geometry import shape, box, mapping
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'bindings/python'))
from raster_engine_lab import Engine,EngineError

def rectangle(x0,y0,x1,y1):
    return mapping(box(x0,y0,x1,y1))

def resident(width=130,height=70,t=0,origin=0):
    yy,xx=np.mgrid[:height,:width]
    values=((xx*3+yy*7+t*11)%31-9).astype(float)
    valid=(xx+yy*2+t)%13!=0
    return {'grid':{'width':width,'height':height,'transform':[origin,1,0,height,0,-1],'crs':'EPSG:3857'},
        'bands':[{'values':values.ravel().tolist(),'valid':valid.ravel().tolist()},
                 {'values':((xx+yy+t)%7+1).ravel().tolist(),'valid':((xx*2+yy+t)%17!=0).ravel().tolist()}]}

def zones():
    return [{'id':'whole','version':'1','geometry':rectangle(0,0,130,70)},
            {'id':'shift','version':'1','geometry':rectangle(11.5,2.25,123.25,68.5)},
            {'id':'hole','version':'1','geometry':mapping(box(3,1,127,69).difference(box(32.25,11.25,86.5,40.5)))},
            {'id':'thin','version':'1','geometry':rectangle(64.25,3,64.25+2**-24,65)},
            {'id':'outside','version':'1','geometry':rectangle(200,100,201,101)}]

def job_for(z=None,slices=None,**kw):
    return dict(zones=z or zones(),slices=slices or [{'id':'d0','source':'r0','time':'2024-01-01'}],
        crs='EPSG:3857',options={'statistics':['sum','support','mean','min','max','count'],'bands':[0]},tile_edge=64,**kw)

def oracle(raster,geometry,band=0):
    g=shape(geometry);w=raster['grid']['width'];h=raster['grid']['height'];x0=raster['grid']['transform'][0]
    b=raster['bands'][band];total=Fraction(0);valid=Fraction(0);selected=Fraction(0);count=0;values=[]
    for y in range(h):
        for x in range(w):
            f=Fraction(g.intersection(box(x+x0,h-y-1,x+x0+1,h-y)).area)
            selected+=f
            if f and b.get('valid',[True]*(w*h))[y*w+x]:
                v=b['values'][y*w+x];total+=f*Fraction(v);valid+=f;count+=1;values.append(v)
    return float(total),float(valid),float(selected),count,min(values) if values else None,max(values) if values else None

@pytest.mark.parametrize('layout',['auto','compact','compact_shared','compact_rows','csr'])
@pytest.mark.parametrize('schedule',['feature','tile','mixed'])
def test_batch_independent_geometry_mask_and_shared_reads(layout,schedule):
    rasters=[resident(t=t) for t in range(3)];z=zones()
    expected=[oracle(r,g['geometry']) for r in rasters for g in z]
    with Engine() as e:
        for i,r in enumerate(rasters):e.open_raster(r,id=f'r{i}')
        job=job_for(slices=[{'id':f'd{i}','source':f'r{i}','time':f'2024-01-0{i+1}'} for i in range(3)],geometry_layout=layout,schedule=schedule)
        pages=list(e.batch_pages(job,max_rows=2));rows=[r for p in pages for r in p['rows']]
    assert len(rows)==15 and len({r['result_id'] for r in rows})==15
    for row,(total,valid,selected,count,minimum,maximum) in zip(rows,expected):
        b=row['bands'][0]
        assert b['fractional_sum']==pytest.approx(total,abs=1e-10,rel=1e-12)
        assert b['covered_cell_equivalents']==pytest.approx(valid,abs=1e-12,rel=1e-12)
        assert b['selected_cell_equivalents']==pytest.approx(selected,abs=1e-12,rel=1e-12)
        assert b['valid_cell_count']==count
        assert b['min']==minimum and b['max']==maximum
        if valid: assert b['coverage_weighted_mean']==pytest.approx(total/valid,abs=1e-11,rel=1e-12)
    m=pages[-1]['metrics']
    assert m['geometry_compilations']==5 and m['geometry_cache_hits']==10
    if schedule!='feature':assert m['windows_read']==18
    else:assert m['windows_read']>18
    assert all(len(p['rows'])<=2 for p in pages)

@pytest.mark.parametrize('layout',['auto','compact_rows'])
def test_exact_row_reuse_and_dynamic_range_fallback(layout):
    # Different proper subranges: no whole-grid-summary shortcut, and prefix
    # subtraction must retain small values after a large exact prefix. Sixteen
    # consumers exercise the revised conservative automatic cost threshold.
    cases = [[2.**70, -2.**70] + [float(i % 7 - 3) for i in range(62)],
             [2.**500] + [2.**-500] * 63]
    for values in cases:
        r={'grid':{'width':64,'height':3,'transform':[0,1,0,3,0,-1],'crs':'EPSG:3857'},'bands':[{'values':values*3}]}
        z=[{'id':str(i),'version':'1','geometry':rectangle(2+i,0,62-i,3)} for i in range(16)]
        with Engine() as e:
            e.open_raster(r,id='r0')
            page=list(e.batch_pages(job_for(z=z,geometry_layout=layout)))[0]
        for i,row in enumerate(page['rows']):
            expected=float(3*sum(map(Fraction,values[2+i:62-i]),Fraction(0)))
            assert row['bands'][0]['fractional_sum']==expected
        m=page['metrics']
        if values[0]==2.**70:
            assert m['row_prefix_builds']==1 and m['row_prefix_ranges']==16
            assert m['row_prefix_input_visits']==128
        else:
            assert m['row_prefix_rejections']==1 and m['row_range_fallbacks']==16

def test_auto_keeps_single_consumer_spans_direct():
    with Engine() as e:
        e.open_raster(resident(),id='r0')
        page=list(e.batch_pages(job_for(z=zones()[:1],geometry_layout='auto')))[0]
    assert page['metrics']['row_prefix_builds']==0
    assert page['metrics']['row_direct_spans']>0


def test_grid_identity_resume_backpressure_and_stable_ids():
    with Engine() as e:
        e.open_raster(resident(),id='r0');e.open_raster(resident(t=1,origin=.5),id='r1')
        job=job_for(slices=[{'id':'d0','source':'r0'},{'id':'d1','source':'r1'}])
        e.call(dict(op='start_job',id='first',job=job))
        a=e.call(dict(op='next_job',id='first',max_rows=2))
        # Merely inspecting status cannot scan the next date or drain buffered rows.
        assert e.call(dict(op='job_info',id='first'))['next_row']==2
        b=e.call(dict(op='next_job',id='first',max_rows=2))
        assert b['metrics']['windows_read']==a['metrics']['windows_read']
        e.call(dict(op='close_job',id='first'))
        resumed=list(e.batch(job,id='resumed',checkpoint=a['checkpoint'],max_rows=1))
        complete=list(e.batch(job,id='whole'))
        assert [r['result_id'] for r in a['rows']+resumed]==[r['result_id'] for r in complete]
        assert len({r['grid_id'] for r in complete})==2
        e.call(dict(op='close',source='r0'));e.open_raster(resident(t=8),id='r0')
        with pytest.raises(EngineError,match='version changed'):list(e.batch(job,id='changed',checkpoint=a['checkpoint']))

def test_fused_normalized_difference_mask_and_mean_of_ratio():
    raster={'grid':{'width':4,'height':1,'transform':[0,1,0,1,0,-1],'crs':'EPSG:3857'},
        'bands':[{'values':[8,3,0,4]},{'values':[2,1,0,None]}]}
    z=[{'id':'z','version':'1','geometry':rectangle(0,0,4,1)}]
    expression={'op':'normalized_difference','left':{'op':'band','band':0},'right':{'op':'band','band':1}}
    mask={'op':'greater','left':{'op':'band','band':0},'right':{'op':'constant','value':1}}
    with Engine() as e:
        e.open_raster(raster,id='r0')
        job=job_for(z=z,expression=expression,mask=mask)
        row=list(e.batch(job))[0]['bands'][0]
    assert row['coverage_weighted_mean']==pytest.approx(.55,abs=1e-15)
    assert row['covered_cell_equivalents']==2
    assert row['coverage_weighted_mean']!=pytest.approx((11-3)/(11+3))

def test_mixed_original_index_shared_leaf_and_raw_fallback(tmp_path):
    path=tmp_path/'source.tif';index=tmp_path/'index';r=resident(193,193);a=np.asarray(r['bands'][0]['values']).reshape(193,193)
    a[~np.asarray(r['bands'][0]['valid']).reshape(193,193)]=-9999
    with rasterio.open(path,'w',driver='GTiff',width=193,height=193,count=1,dtype='float64',nodata=-9999,crs='EPSG:3857',transform=Affine(1,0,0,0,-1,193),tiled=True,blockxsize=64,blockysize=64) as ds:ds.write(a,1)
    with Engine() as e:
        e.register_source({'location':str(path)},id='s')
        built=e.prepare_source(index,source='s',tile_edge=64)
        z=[{'id':f'z{i}','version':'1','geometry':rectangle(.25+i*.125,.25,192.75,192.75)} for i in range(5)]
        job=job_for(z=z,slices=[{'id':'d','source':'s','index':str(index/'summary.rsi'),'expected_build_id':built['build_id']}],schedule='mixed')
        pages=list(e.batch_pages(job));rows=pages[0]['rows'];m=pages[0]['metrics']
        assert m['summary_records_read']>0 and m['summarized_polygon_tiles']>=5*m['summary_records_read']
        assert m['windows_read']<16
        for zone,row in zip(z,rows):
            direct=e.measure_source(zone['geometry'],'EPSG:3857',source='s',bands=[0],statistics=job['options']['statistics'])
            assert row['bands']==direct['bands']
        job['options']={'bands':[0],'statistics':['variance','stddev']}
        fallback=list(e.batch_pages(job))[0]
        assert fallback['metrics']['summary_records_read']==0 and fallback['metrics']['windows_read']==16

def test_budget_unknown_fields_and_no_partial_success():
    with Engine() as e:
        e.open_raster(resident(),id='r0')
        bad=job_for();bad['zones'][1]['id']='whole'
        with pytest.raises(EngineError,match='unique'):list(e.batch(bad))
        bad=job_for();bad['budget']={'workers':2}
        with pytest.raises(EngineError,match='one native worker'):list(e.batch(bad))
        bad=job_for();bad['expression']={'op':'python','code':'print(1)'}
        with pytest.raises(EngineError):list(e.batch(bad))
        bad=job_for();bad['budget']={'max_windows':1}
        e.call(dict(op='start_job',id='limited',job=bad))
        with pytest.raises(EngineError,match='IO budget'):e.call(dict(op='next_job',id='limited'))
        assert e.call(dict(op='job_info',id='limited'))['next_row']==0
        e.call(dict(op='close_job',id='limited'))
        bad=job_for();bad['options']={'statistics':['median'],'quantile_max_samples':4}
        e.call(dict(op='start_job',id='quantile',job=bad))
        with pytest.raises(EngineError,match='sample'):e.call(dict(op='next_job',id='quantile'))
        assert e.call(dict(op='job_info',id='quantile'))['next_row']==0
        e.call(dict(op='close_job',id='quantile'))
