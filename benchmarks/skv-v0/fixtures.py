#!/usr/bin/env python3
"""Bounded independent logical fixtures and competent native-resolution layouts.

Use the system Python/GDAL that matches Skarve's native GDAL for encoding.
No network activity; optional real sources are supplied explicitly by the caller.
"""
from __future__ import annotations
import argparse, hashlib, json, time
from pathlib import Path
import numpy as np
from osgeo import gdal, osr

gdal.UseExceptions()
gdal.SetCacheMax(64<<20)
gdal.SetConfigOption('GDAL_NUM_THREADS','1')
gdal.SetConfigOption('PROJ_NETWORK','OFF')

def sha(path):
    with Path(path).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()

def logical(path):
    ds=gdal.Open(str(path));bands=[]
    for b in range(1,ds.RasterCount+1):
        band=ds.GetRasterBand(b);raw=hashlib.sha256();mask=hashlib.sha256();valid_count=0
        for y in range(0,ds.RasterYSize,64):
            height=min(64,ds.RasterYSize-y)
            values=band.ReadAsArray(0,y,ds.RasterXSize,height)
            validity=band.GetMaskBand().ReadAsArray(0,y,ds.RasterXSize,height)
            raw.update(values.astype(values.dtype.newbyteorder('<'),copy=False).tobytes())
            mask.update(validity.tobytes());valid_count+=int(np.count_nonzero(validity))
        bands.append({'raw_sha256':raw.hexdigest(),'mask_sha256':mask.hexdigest(),
            'valid_mask_cells':valid_count,'type':gdal.GetDataTypeName(band.DataType),
            'nodata':band.GetNoDataValue(),'scale':band.GetScale() or 1.0,
            'offset':band.GetOffset() or 0.0,'unit':band.GetUnitType(),
            'description':band.GetDescription()})
    return {'width':ds.RasterXSize,'height':ds.RasterYSize,'transform':ds.GetGeoTransform(),
            'crs':ds.GetProjection(),'pixel_convention':ds.GetMetadataItem('AREA_OR_POINT'),'bands':bands}

def layout(path):
    ds=gdal.Open(str(path));band=ds.GetRasterBand(1)
    return {'block':band.GetBlockSize(),'image_structure':ds.GetMetadata('IMAGE_STRUCTURE'),
            'overviews':band.GetOverviewCount(),'bytes':Path(path).stat().st_size,'sha256':sha(path)}

def synthetic(path, count, width=513, height=519, seed=17391):
    assert not path.exists()
    ds=gdal.GetDriverByName('GTiff').Create(str(path),width,height,count,gdal.GDT_Float32,
        ['TILED=NO','BLOCKYSIZE=16','COMPRESS=DEFLATE','PREDICTOR=3','INTERLEAVE=BAND','NUM_THREADS=1'])
    ds.SetGeoTransform((0,1,0,height,0,-1));crs=osr.SpatialReference();crs.ImportFromEPSG(3857);ds.SetProjection(crs.ExportToWkt());ds.SetMetadataItem('AREA_OR_POINT','Area')
    y,x=np.indices((height,width),dtype=np.int64)
    for b in range(count):
        # Dyadic Float32 values, distinct structured and high-frequency band terms.
        values=(((x*(17+2*b)+y*(31+3*b)+(x*y)%(97+b)+seed+113*b)%8192)/16.0 + b*29).astype('float32')
        values[(x+7*y+b*13)%61==0]=-99999
        values[((x//31+3*y//23+b)%19)==0]=0
        values[(x*3+y*7+b)%89==0]*=-1
        band=ds.GetRasterBand(b+1);band.WriteArray(values);band.SetNoDataValue(-99999)
        band.SetDescription(f'independent_band_{b:02d}');band.SetUnitType('analytical_value')
    ds=None

def encode(source, output, *, kind, block=256):
    assert not output.exists()
    options=['NUM_THREADS=1']
    if kind=='cog':options+=['COMPRESS=ZSTD','LEVEL=6','PREDICTOR=FLOATING_POINT',f'BLOCKSIZE={block}','OVERVIEWS=NONE','STATISTICS=NO'];driver='COG'
    elif kind=='band_tiled':options+=['TILED=YES',f'BLOCKXSIZE={block}',f'BLOCKYSIZE={block}','COMPRESS=ZSTD','ZSTD_LEVEL=6','PREDICTOR=3','INTERLEAVE=BAND'];driver='GTiff'
    else:options+=['TILED=NO','BLOCKYSIZE=16','COMPRESS=DEFLATE','PREDICTOR=3','INTERLEAVE=BAND'];driver='GTiff'
    start=time.perf_counter();dataset=gdal.Translate(str(output),str(source),format=driver,creationOptions=options);dataset=None
    return {'elapsed_seconds':time.perf_counter()-start,'creation_options':options,'driver':driver,**layout(output)}

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--age-crop',type=Path);p.add_argument('--population',type=Path)
    p.add_argument('--seed',type=int,default=17391);p.add_argument('--whole-population-only',action='store_true')
    p.add_argument('--width',type=int,default=513);p.add_argument('--height',type=int,default=519);p.add_argument('--bands',type=int,nargs='+',default=[36,40]);args=p.parse_args()
    args.output.mkdir(parents=True,exist_ok=True);manifest={'schema':'skarve_skv_fixtures_v1','gdal':gdal.VersionInfo('--version'),'seed':args.seed,'sources':[]}
    specs=[]
    for bands in (() if args.whole_population_only else args.bands):
        name=f'analytical{bands}'+(f'-{args.width}x{args.height}' if (args.width,args.height)!=(513,519) else '')
        path=args.output/f'{name}-ordinary.tif';start=time.perf_counter();synthetic(path,bands,width=args.width,height=args.height,seed=args.seed)
        specs.append((name,path,{'kind':'independently_authored_generated','seconds':time.perf_counter()-start}))
    if args.age_crop and not args.whole_population_only:
        path=args.output/'real-age36-ordinary.tif';conversion=encode(args.age_crop,path,kind='ordinary')
        specs.append(('real-age36',path,{'kind':'private_actual_native_crop','input_sha256':sha(args.age_crop),'crop_window_xywh':[2048,512,512,512],'original_grid':[4634,2408],'conversion':conversion}))
    if args.population:
        path=args.output/'worldpop1-ordinary.tif';source=gdal.Open(str(args.population));gt=source.GetGeoTransform()
        center_x=int((-1.52-gt[0])/gt[1]);center_y=int((12.37-gt[3])/gt[5]);window=([0,0,source.RasterXSize,source.RasterYSize] if args.whole_population_only else [center_x-512,center_y-512,1024,1024])
        start=time.perf_counter();ds=gdal.Translate(str(path),source,format='GTiff',srcWin=window,creationOptions=['TILED=NO','BLOCKYSIZE=16','COMPRESS=DEFLATE','PREDICTOR=3','NUM_THREADS=1']);ds=None
        specs.append(('worldpop1',path,{'kind':'existing_worldpop_native_crop','input_sha256':sha(args.population),'crop_window_xywh':window,'original_grid':[source.RasterXSize,source.RasterYSize],'seconds':time.perf_counter()-start}))
    for name,path,origin in specs:
        base=logical(path);variants={'ordinary':{'path':path.name,**layout(path)}}
        for label,kind,block in [('cog128','cog',128),('cog256','cog',256),('band256','band_tiled',256)]:
            output=args.output/f'{name}-{label}.tif';encoded=encode(path,output,kind=kind,block=block)
            alternative=logical(output)
            assert alternative==base,(name,label,'logical raster changed')
            variants[label]={'path':output.name,**encoded,'logical_match':True}
        distinct=len({b['raw_sha256'] for b in base['bands']});assert distinct==len(base['bands'])
        manifest['sources'].append({'id':name,'origin':origin,'logical':base,'distinct_bands':distinct,'variants':variants})
        print(json.dumps({'dataset':name,'bands':distinct,'variants':{k:v['bytes'] for k,v in variants.items()}}),flush=True)
    manifest['scratch_bytes']=sum(p.stat().st_size for p in args.output.rglob('*') if p.is_file())
    (args.output/'fixtures.json').write_text(json.dumps(manifest,indent=2,allow_nan=False)+'\n')

if __name__=='__main__':main()
