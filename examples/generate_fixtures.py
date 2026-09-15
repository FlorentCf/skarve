#!/usr/bin/env python3
"""Generate small original GeoTIFFs and a COG; no downloaded data or credentials."""
import argparse
import hashlib
import json
import math
from pathlib import Path


def rectangle(x0, y0, x1, y1):
    return {'type': 'Polygon', 'coordinates': [[[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]]}


def rectangle_oracle(values, valid, scales, offsets, bounds):
    """Independent axis-aligned rectangle/cell intersection and binary64 fsum."""
    x0,y0,x1,y1 = bounds
    height,width = values.shape[1:]
    answer = []
    for band in range(values.shape[0]):
        masses, products, selected_values = [], [], []
        for row in range(max(0, math.floor(height-y1)), min(height, math.ceil(height-y0))):
            wy = max(0., min(height-row,y1)-max(height-row-1,y0))
            for column in range(max(0, math.floor(x0)), min(width, math.ceil(x1))):
                wx = max(0., min(column+1,x1)-max(column,x0))
                fraction = wx*wy
                if fraction > 0 and bool(valid[band,row,column]):
                    value = float(values[band,row,column])*scales[band]+offsets[band]
                    masses.append(fraction); products.append(fraction*value); selected_values.append(value)
        support,total = math.fsum(masses),math.fsum(products)
        answer.append({'band': band, 'fractional_sum': total, 'covered_cell_equivalents': support,
                       'coverage_weighted_mean': total/support if support else None,
                       'min': min(selected_values) if selected_values else None,
                       'max': max(selected_values) if selected_values else None,
                       'valid_cell_count': len(selected_values)})
    return answer


def generate(folder):
    import numpy as np
    import rasterio
    from rasterio.shutil import copy as raster_copy
    from rasterio.transform import from_origin
    folder = Path(folder)
    folder.mkdir(parents=True, exist_ok=False)
    size = 64
    yy,xx = np.indices((size,size))
    nodata = -9999.
    scales,offsets = [.5,2.,-1.],[10.,-4.,3.]
    shapes = {'whole': [0.,0.,64.,64.], 'small': [.25,50.5,12.75,63.25],
              'overlap': [5.5,45.25,21.25,60.5], 'other': [34.25,1.5,57.75,20.25],
              'thin': [1.,60.,1.+1e-9,61.], 'outside': [-2.,-2.,-1.,-1.]}
    sources = {}
    for file_id,delta in [('original',0),('date-b',7),('mismatch',100)]:
        values = np.stack([((xx+3*yy+11*b)%61)-20+b*2+delta for b in range(3)]).astype('float32')
        dataset_mask = (xx+yy)%17 != 0
        valid = np.stack([dataset_mask & ((xx+2*yy+b)%31 != 0) for b in range(3)])
        values[~np.stack([((xx+2*yy+b)%31 != 0) for b in range(3)])] = nodata
        path = folder/(file_id+'.tif')
        with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
            with rasterio.open(path,'w',driver='GTiff',width=size,height=size,count=3,dtype='float32',
                               crs='EPSG:3857',transform=from_origin(0,size,1,1),nodata=nodata,
                               tiled=True,blockxsize=16,blockysize=16,compress='deflate') as dataset:
                dataset.write(values); dataset.write_mask(dataset_mask.astype('uint8')*255)
                dataset.scales=tuple(scales); dataset.offsets=tuple(offsets)
        sources[file_id] = {'file': path.name, 'bytes': path.stat().st_size,
            'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'bands':3,
            'expected': {name: rectangle_oracle(values,valid,scales,offsets,bounds)
                         for name,bounds in shapes.items()}}
    size = 512
    yy,xx = np.indices((size,size))
    values = ((np.arange(size*size).reshape(size,size)%317)/16.-5.).astype('float32')[None,:,:]
    valid = ((xx+yy)%79 != 0)[None,:,:]
    values[~valid] = nodata
    original,cog = folder/'range-original.tif',folder/'range.cog.tif'
    with rasterio.open(original,'w',driver='GTiff',width=size,height=size,count=1,dtype='float32',
                       crs='EPSG:3857',transform=from_origin(0,size,1,1),nodata=nodata,
                       tiled=True,blockxsize=128,blockysize=128) as dataset:
        dataset.write(values)
    raster_copy(original,cog,driver='COG',BLOCKSIZE=128,COMPRESS='NONE',OVERVIEWS='NONE')
    remote_shapes = {'hot-a':[3.25,480.5,8.5,488.], 'hot-b':[12.5,474.25,20.75,483.5],
                     'hot-c':[25.25,469.5,34.75,481.25], 'far-a':[300.25,480.5,305.5,488.],
                     'far-b':[300.25,30.5,305.5,38.], 'far-c':[3.25,30.5,8.5,38.]}
    manifest = {'schema':1, 'origin':'Deterministically generated mathematical test data; no external dataset.',
        'crs':'EPSG:3857', 'sources':sources,
        'geometries':{name:rectangle(*bounds) for name,bounds in shapes.items()},
        'remote':{'file':cog.name,'bytes':cog.stat().st_size,'sha256':hashlib.sha256(cog.read_bytes()).hexdigest(),
            'geometries':{name:rectangle(*bounds) for name,bounds in remote_shapes.items()},
            'expected':{name:rectangle_oracle(values,valid,[1.],[0.],bounds) for name,bounds in remote_shapes.items()}},
        'dependencies':{'numpy':np.__version__,'rasterio':rasterio.__version__,'fixture_gdal':rasterio.__gdal_version__}}
    (folder/'fixture.json').write_text(json.dumps(manifest,indent=2,allow_nan=False)+'\n')
    (folder/'polygon.json').write_text(json.dumps(manifest['geometries']['small'])+'\n')
    return manifest


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('output',type=Path)
    args=parser.parse_args();result=generate(args.output)
    print(json.dumps({'fixture':'fixture.json','source_count':len(result['sources'])+1,'network_requests':0}))
