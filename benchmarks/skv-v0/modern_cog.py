#!/usr/bin/env python3
"""Explicit stronger COG control using the already installed Rasterio GDAL.

Does not replace native/system GDAL. Native Skarve keeps its same reader build
for all inputs. Adds BAND and TILE COG encodings where this encoder supports
them; verify all raw/mask bytes and interpretation independently on reopening.
"""
import argparse, hashlib, json, os, time
from pathlib import Path
os.environ['GDAL_NUM_THREADS']='1';os.environ['GDAL_CACHEMAX']='64'
import rasterio
from rasterio.shutil import copy

def digest(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def compare(original,derived):
    with rasterio.open(original) as a,rasterio.open(derived) as b:
        assert (a.width,a.height,a.count,a.dtypes,a.transform,a.crs,a.scales,a.offsets,a.nodatavals,a.units,a.descriptions)==(b.width,b.height,b.count,b.dtypes,b.transform,b.crs,b.scales,b.offsets,b.nodatavals,b.units,b.descriptions)
        for band in range(1,a.count+1):
            for y in range(0,a.height,64):
                window=rasterio.windows.Window(0,y,a.width,min(64,a.height-y))
                assert a.read(band,window=window).tobytes()==b.read(band,window=window).tobytes()
                assert a.read_masks(band,window=window).tobytes()==b.read_masks(band,window=window).tobytes()
        return {'width':b.width,'height':b.height,'bands':b.count,'block':b.block_shapes[0],'image_structure':b.tags(ns='IMAGE_STRUCTURE'),'overviews':b.overviews(1)}

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--fixtures',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--datasets');p.add_argument('--block',type=int,default=128);args=p.parse_args()
    assert tuple(int(v) for v in rasterio.__gdal_version__.split('.')[:2])>=(3,11),'COG BAND/TILE encoder unavailable'
    assert not args.output.exists(),'No overwrite';manifest=json.loads(args.fixtures.read_text());directory=args.fixtures.parent;selected=set(args.datasets.split(',')) if args.datasets else None
    for source in manifest['sources']:
        if selected and source['id'] not in selected:continue
        original=directory/source['variants']['ordinary']['path']
        for interleave in ('BAND','TILE'):
            label=f'cog{interleave.lower()}{args.block}';path=directory/f'{source["id"]}-{label}.tif';assert not path.exists()
            options={'driver':'COG','compress':'ZSTD','level':6,'predictor':'FLOATING_POINT','blocksize':args.block,'overviews':'NONE','statistics':'NO','interleave':interleave,'num_threads':1}
            start=time.perf_counter()
            with rasterio.Env(GDAL_CACHEMAX=64<<20):copy(original,path,**options)
            elapsed=time.perf_counter()-start;verified=compare(original,path)
            assert verified['image_structure'].get('LAYOUT')=='COG' and not verified['overviews']
            source['variants'][label]={'path':path.name,'bytes':path.stat().st_size,'sha256':digest(path),'elapsed_seconds':elapsed,'logical_match':True,'encoder':f'Rasterio {rasterio.__version__} bundled GDAL {rasterio.__gdal_version__}','creation_options':options,**verified}
            print(json.dumps({'dataset':source['id'],'variant':label,'bytes':path.stat().st_size,'ms':elapsed*1000}),flush=True)
    args.output.write_text(json.dumps(manifest,indent=2,allow_nan=False)+'\n')

if __name__=='__main__':main()
