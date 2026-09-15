#!/usr/bin/env python3
"""Installed source-owned direct, optional-index and paged date-stack example."""
import argparse
import json
from pathlib import Path
from skarve import Skarve

STATISTICS=['sum','support','mean','min','max','count']


def make_job(folder,fixture):
    zones=[{'id':name,'version':'1','geometry':fixture['geometries'][name]}
           for name in ('small','overlap','other')]
    slices=[{'id':f'{source}-band{band}','time':f'synthetic-{source}',
             'spec':{'location':str(folder/fixture['sources'][source]['file'])},'bands':[band]}
            for source in ('original','date-b') for band in (2,0,1)]
    return {'zones':zones,'slices':slices,'crs':fixture['crs'],'tile_edge':32,
            'budget':{'working_bytes':96*1024**2,'geometry_bytes':8*1024**2,
                      'tile_bytes':8*1024**2,'output_bytes':1024**2,
                      'max_windows':64,'decoded_bytes':8*1024**2,
                      'max_contributions':1024**2,'workers':1},
            'options':{'statistics':STATISTICS}}


def run(fixture_path,index_path):
    fixture_path=Path(fixture_path);folder=fixture_path.parent
    fixture=json.loads(fixture_path.read_text())
    geometry=fixture['geometries']['small']
    with Skarve() as engine:
        engine.call({'op':'configure_file_cache','bytes':1024**2})
        with engine.infuse(folder/fixture['sources']['original']['file'],id='elevation') as source:
            direct=source.carve(zone=geometry,crs=fixture['crs'],bands=[0],metrics=STATISTICS)
            built=source.ward(index_path,boundary_source='original',tile_edge=16)
            with source.open_index(Path(index_path)/'summary.rsi',expected_build_id=built['build_id']) as index:
                indexed=index.measure(geometry,fixture['crs'],bands=[0],statistics=STATISTICS)
            assert direct['bands']==indexed['bands']
        rows=0;last=None
        for page in engine.cleave(make_job(folder,fixture),max_rows=2):
            # Consume each page here; never collect a complete polygon-by-date output.
            assert len(page['rows'])<=2
            rows+=len(page['rows']);last=page
        assert last['complete'] and rows==18
        return {'interface':'Python','direct':direct['bands'],'indexed_equal':True,'batch_rows':rows,
                'batch_metrics':last['metrics'],'index_bytes':built['summary_bytes'],
                'duplicate_raw_bytes':built['duplicate_raw_bytes']}


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture',required=True,type=Path);parser.add_argument('--index',required=True,type=Path)
    args=parser.parse_args();print(json.dumps(run(args.fixture,args.index),allow_nan=False))
