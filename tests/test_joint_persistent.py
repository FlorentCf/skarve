"""Public persistent C1 execution checked against independent GEOS/Fraction areas."""
import copy
import sys
from fractions import Fraction
from pathlib import Path
import numpy as np
import pytest
import rasterio
from rasterio.transform import from_origin
from shapely.geometry import box, mapping, MultiPolygon, Polygon, shape
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'bindings/python'))
from raster_engine_lab import Engine, EngineError

STATISTICS=['sum','support','mean','min','max','count']
def fixture(path,width=65,height=49):
    y,x=np.mgrid[:height,:width]
    values=np.stack([((x*7+y*3+b*11)%47-19).astype(float)/4 for b in range(3)])
    valid=np.stack([(x+2*y+b*5)%17!=0 for b in range(3)])
    with rasterio.open(path,'w',driver='GTiff',width=width,height=height,count=3,dtype='float64',crs='EPSG:3857',transform=from_origin(0,height,1,1),nodata=-9999) as ds:
        ds.write(np.where(valid,values,-9999))
    return values,valid

def polygons():
    return [box(-1,-1,66,50),box(2.25,1.75,61.5,47.25),
        box(0,0,64,48).difference(box(17.5,16.25,46.5,31.5)),
        MultiPolygon([box(0,33,32,49),box(48,1,64,17)]),
        box(31.25,3,31.25+2**-30,45),box(100,100,101,101),
        Polygon([(0,0),(64,0),(51.5,48),(13.25,48),(0,0)]),box(0,0,65,49)]

def oracle(values,valid,poly,bands=(2,0)):
    cells=[]
    for y in range(values.shape[1]):
        for x in range(values.shape[2]):
            area=poly.intersection(box(x,values.shape[1]-y-1,x+1,values.shape[1]-y)).area
            if area>0:cells.append((y,x,Fraction(area)))
    expected=[]
    for b in bands:
        good=[(Fraction(float(values[b,y,x])),f) for y,x,f in cells if valid[b,y,x]]
        total=sum((v*f for v,f in good),Fraction(0));support=sum((f for _,f in good),Fraction(0))
        mass=sum((abs(v)*f for v,f in good),Fraction(0));selected=sum((f for _,_,f in cells),Fraction(0))
        expected.append(dict(fractional_sum=float(total),covered_cell_equivalents=float(support),selected_cell_equivalents=float(selected),valid_cell_count=len(good),min=min((float(v) for v,_ in good),default=None),max=max((float(v) for v,_ in good),default=None),coverage_weighted_mean=float(total/support) if support else None,mass=float(mass)))
    return expected

def check(result,expected):
    for actual,want in zip(result['bands'],expected):
        for key in ['min','max','valid_cell_count']:
            assert actual[key]==want[key],(key,actual,want)
        for key in ['fractional_sum','covered_cell_equivalents','selected_cell_equivalents','coverage_weighted_mean']:
            if want[key] is None:assert actual[key] is None
            else:
                tol=1e-8+1e-10*want['mass'] if key=='fractional_sum' else max(1e-12,abs(want[key])*1e-12)
                assert abs(actual[key]-want[key])<=tol,(key,actual[key],want[key],tol)

def config(mode='joint',latency=1_000_000):
    return dict(mode=mode,model=dict(request_latency_ns=latency,bandwidth_bytes_per_second=100_000_000),max_range_bytes=65536,max_summary_gap_bytes=65536,summary_reduction_ns=50,raw_cell_reduction_ns=5)

def protected(poly):
    result=[]
    for ty in range(4):
        for tx in range(5):
            tile=box(tx*16,max(0,49-(ty+1)*16),min(65,(tx+1)*16),49-ty*16)
            if poly.contains(tile) and not poly.boundary.intersects(tile):result.append(ty*5+tx)
    return result

def inspect_io(result,config,forbidden,groups):
    jp=result['joint_planner'];io=result['io'];records=[r for r in io['records'] if r['offset']!=0]
    assert len(records)==jp['cost']['requests']==len(jp['ranges'])
    assert sum(r['length'] for r in records)==jp['cost']['fetched_bytes']
    assert all(r['length']<=config['max_range_bytes'] for r in records)
    assert jp['search']['planner_buffer_bound']<=config.get('limits',{}).get('max_planner_bytes',8*1024*1024)
    assert result['work']['buffer_bound_bytes']<=result['work']['read_memory_budget_bytes']
    raw_size=16*16*(3 if groups==1 else 1)*9+32
    for r in records:
        if r['source']=='raw':
            assert (r['offset']-65536)%raw_size==0 and r['length']%raw_size==0
            pages=range((r['offset']-65536)//raw_size,(r['offset']-65536+r['length'])//raw_size)
            assert all(page//groups not in forbidden for page in pages)
    assert isinstance(jp['cost']['score_numerator'],str)

@pytest.mark.parametrize('layout',['band_major','cell_major'])
def test_joint_executor_actual_choices_ranges_and_independent_stats(tmp_path,layout):
    path=tmp_path/'input.tif';values,valid=fixture(path);dest=tmp_path/'index'
    with Engine() as engine:
        engine.prepare_file(path,dest,tile_edge=16,layout=layout,summary_backend='hierarchy',boundary_source='normalized')
        for poly in polygons():
            wanted=oracle(values,valid,poly);forbidden=protected(poly)
            base=dict(op='measure_file',index=str(dest),geometry=mapping(poly),crs='EPSG:3857',bands=[2,0],statistics=STATISTICS,read_memory_bytes=32*1024*1024)
            for mode in ['joint','greedy','fixed_request_first']:
                cfg=config(mode);result=engine.call(dict(base,joint_planner=cfg,forbidden_raw_tiles=forbidden))
                check(result,wanted);inspect_io(result,cfg,forbidden,3 if layout=='band_major' else 1)
                if mode=='joint':assert result['joint_planner']['exact']
            for backend in ['persistent_flat','persistent_hierarchy','direct']:
                check(engine.call(dict(base,backend=backend)),wanted)

def test_joint_executor_rejections_and_explicit_beam_flag(tmp_path):
    path=tmp_path/'input.tif';fixture(path);dest=tmp_path/'index'
    with Engine() as engine:
        engine.prepare_file(path,dest,tile_edge=16,layout='band_major',summary_backend='hierarchy',boundary_source='normalized')
        base=dict(op='measure_file',index=str(dest),geometry=mapping(box(-1,-1,66,50)),crs='EPSG:3857',statistics=STATISTICS)
        cfg=config();cfg['limits']={'max_states':1,'beam_width':1}
        result=engine.call(dict(base,joint_planner=cfg))
        assert not result['joint_planner']['exact'] and result['joint_planner']['search']['state_budget_exhausted']
        for extra,pattern in [({'read_memory_bytes':1024*1024},'buffers exceed'),({'backend':'direct'},'requires eligible'),({'backend':'persistent_flat'},'forced flat'),({'joint_planner':dict(config(),typo=1)},'unknown field'),({'statistics':['variance']},'requires eligible')]:
            with pytest.raises(EngineError,match=pattern):engine.call(dict(base,joint_planner=config())|extra)
        cfg=config();cfg['limits']={'max_planner_bytes':65536,'beam_width':128}
        with pytest.raises(EngineError,match='reservation|memory'):engine.call(dict(base,joint_planner=cfg))
        cfg=config();cfg['max_range_bytes']=8
        with pytest.raises(EngineError,match='span'):engine.call(dict(base,joint_planner=cfg))
        original=tmp_path/'original'
        engine.prepare_file(path,original,tile_edge=16,layout='band_major',summary_backend='hierarchy',boundary_source='original')
        with pytest.raises(EngineError,match='original encoded source mapping unavailable'):
            engine.call(dict(base,index=str(original),path=str(path),joint_planner=config()))


@pytest.mark.parametrize("summary_gap",[0,65536])
def test_native_row_layout_really_changes_cover_and_executes_refinement(tmp_path,summary_gap):
    path=tmp_path/'row.tif';values,valid=fixture(path,width=65,height=15);dest=tmp_path/'index'
    poly=box(-1,-1,48.25,16);want=oracle(values,valid,poly)
    with Engine() as engine:
        engine.prepare_file(path,dest,tile_edge=16,layout='band_major',summary_backend='hierarchy',boundary_source='normalized')
        base=dict(op='measure_file',index=str(dest),geometry=mapping(poly),crs='EPSG:3857',bands=[2,0],statistics=STATISTICS,forbidden_raw_tiles=[0,1,2])
        results={}
        for mode in ['joint','greedy','fixed_request_first']:
            cfg=config(mode);cfg['max_summary_gap_bytes']=summary_gap
            r=engine.call(dict(base,joint_planner=cfg));check(r,want);inspect_io(r,cfg,[0,1,2],3);results[mode]=r
        joint=results['joint']['joint_planner'];greedy=results['greedy']['joint_planner'];fixed=results['fixed_request_first']['joint_planner']
        assert joint['exact'] and joint['summary_actions']==3
        assert greedy['summary_actions']==fixed['summary_actions']==2
        assert joint['cost']['requests']+int(summary_gap==0)==greedy['cost']['requests']==fixed['cost']['requests']
        assert int(joint['cost']['score_numerator'])<int(greedy['cost']['score_numerator'])
        assert joint['cost']['fetched_bytes']==greedy['cost']['fetched_bytes']+(152 if summary_gap==0 else -152)
        assert results['joint']['io']['raw_bytes']==results['greedy']['io']['raw_bytes']
