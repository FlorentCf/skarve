import copy,sys
from pathlib import Path
import numpy as np,rasterio
from rasterio.transform import from_origin
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'bindings/python'))
from raster_engine_lab import Engine

def test_layout_policy_preserves_output_and_cached_grid_across_storage(tmp_path):
    paths=[]
    for edge in (128,512):
        p=tmp_path/f'{edge}.tif';paths.append(p)
        with rasterio.open(p,'w',driver='GTiff',width=641,height=449,count=1,dtype='int32',crs='EPSG:3857',transform=from_origin(0,449,1,1),tiled=True,blockxsize=edge,blockysize=edge,compress='deflate') as ds:ds.write(np.arange(641*449,dtype=np.int32).reshape(449,641),1)
    with Engine() as e:
        for i,p in enumerate(paths):e.register_source({'location':str(p)},id=f's{i}')
        zones=[{'id':str(i),'version':'1','geometry':{'type':'Polygon','coordinates':[[[.25+i,.25],[600,.25],[600,430],[.25+i,430],[.25+i,.25]]]}} for i in range(16)]
        job={'zones':zones,'slices':[{'id':str(i),'source':f's{i}'} for i in range(2)],'crs':'EPSG:3857','tile_edge':64,'options':{'statistics':['sum','support','mean','min','max','count']}}
        base=list(e.batch(job));pages=list(e.batch_pages(dict(job,window_policy='source_layout')));rows=[r for p in pages for r in p['rows']]
        assert rows==base
        assert pages[-1]['metrics']['window_policy_promotions']==2
        assert pages[-1]['metrics']['windows_read']<100
        assert pages[-1]['metrics']['last_window_policy']['executed_edge']==512
        # Optional recompilation has a real geometry capacity bound and may fall
        # back; full default and explicit fixed remain ordinary control paths.
        assert pages[-1]['metrics']['peak_tracked_bytes']<=512<<20

def test_layout_policy_reuses_equal_physical_date_bands_with_independent_validity(tmp_path):
    width,height=641,449
    yy,xx=np.indices((height,width))
    planes=np.stack([xx+3*yy-600,600-xx-3*yy+7,(xx+3*yy)%97-48]).astype(np.int16)
    valid=np.stack([(xx+2*yy+b)%(5+b)!=0 for b in range(3)])
    nodata=-32768
    path=tmp_path/'physical-date-bands.tif'
    with rasterio.open(path,'w',driver='GTiff',width=width,height=height,count=3,
                       dtype='int16',nodata=nodata,crs='EPSG:3857',
                       transform=from_origin(0,height,1,1),tiled=True,
                       blockxsize=512,blockysize=512,compress='deflate',interleave='pixel') as ds:
        ds.write(np.where(valid,planes,nodata).astype(np.int16))
    rectangles=[(.25,.25,600.5,430.75),(8.75,2.5,631.125,400.25)]
    zones=[];coefficients=[]
    for i,(x0,y0,x1,y1) in enumerate(rectangles):
        zones.append({'id':f'z{i}','version':'1','geometry':{'type':'Polygon',
                      'coordinates':[[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]}})
        # Independent rectangle/cell intersection, including cropped grid edges.
        wx=np.maximum(0.,np.minimum(xx+1,x1)-np.maximum(xx,x0))
        wy=np.maximum(0.,np.minimum(height-yy,y1)-np.maximum(height-yy-1,y0))
        coefficients.append(wx*wy)
    selected=[2,0,1,2]
    job={'zones':zones,'slices':[{'id':f'd{i}','source':'dates','bands':[band]}
                               for i,band in enumerate(selected)],
         'crs':'EPSG:3857','tile_edge':64,
         'options':{'statistics':['sum','support','mean','min','max','count']}}
    with Engine() as engine:
        engine.register_source({'location':str(path)},id='dates')
        fixed_pages=list(engine.batch_pages(job,max_rows=2))
        pages=list(engine.batch_pages(dict(job,window_policy='source_layout'),max_rows=2))
    fixed=[r for page in fixed_pages for r in page['rows']]
    actual=[r for page in pages for r in page['rows']]
    assert actual==fixed
    assert len(actual)==len(zones)*len(selected)
    for row in actual:
        band=selected[int(row['slice_id'][1:])]
        coverage=coefficients[int(row['zone_id'][1:])]
        weights=coverage*valid[band]
        contributing=weights>0
        total=float(np.sum(weights*planes[band],dtype=np.float64))
        support=float(np.sum(weights,dtype=np.float64))
        values=planes[band][contributing]
        answer=row['bands'][0]
        assert answer['band']==band
        assert answer['fractional_sum']==total
        assert answer['covered_cell_equivalents']==support
        assert answer['coverage_weighted_mean']==total/support
        assert answer['valid_cell_count']==int(np.count_nonzero(contributing))
        assert answer['min']==int(values.min())
        assert answer['max']==int(values.max())
    metrics=pages[-1]['metrics']
    assert metrics['window_policy_promotions']==1
    assert metrics['geometry_compilations']==2*len(zones) # base + replacement, once
    assert metrics['geometry_cache_hits']==(len(selected)-1)*len(zones)
    assert metrics['last_window_policy']['executed_edge']==512
    assert metrics['windows_read']<fixed_pages[-1]['metrics']['windows_read']