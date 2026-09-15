#!/usr/bin/env python3
"""Installed-only generated-data numerical, identity and resource checks."""
import argparse
import asyncio
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import sys

import numpy as np
import skarve
import skarve_bulk
import raster_engine_lab
from skarve import Engine,EngineError

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'examples'))
from python_source import make_job,STATISTICS
from range_server import served

FIELDS=('fractional_sum','covered_cell_equivalents','coverage_weighted_mean','min','max','valid_cell_count')


def check_band(actual,expected,band=None):
    assert actual['band']==(expected['band'] if band is None else band)
    for key in FIELDS:
        a,b=actual[key],expected[key]
        if b is None:assert a is None,(key,a,b)
        elif key=='valid_cell_count':assert a==b,(key,a,b)
        else:assert math.isclose(a,b,rel_tol=1e-10,abs_tol=1e-8),(key,a,b)
    if 0<expected['covered_cell_equivalents']<1e-7:
        assert actual['covered_cell_equivalents']>0


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture',type=Path,required=True);parser.add_argument('--scratch',type=Path,required=True)
    args=parser.parse_args();args.scratch.mkdir(parents=True,exist_ok=False)
    fixture=json.loads(args.fixture.read_text());folder=args.fixture.parent
    assert all(not os.environ.get(name) for name in ('PYTHONPATH','SKARVE_LIBRARY','RASTER_ENGINE_LIB','RASTER_ENGINE_LIBRARY'))
    for module in (skarve,skarve_bulk,raster_engine_lab):
        assert Path(module.__file__).resolve().is_relative_to(Path(sys.prefix).resolve()),'Binding is not installed in this consumer venv'
    checks=[]
    def passed(name,**details):checks.append(dict(name=name,passed=True,**details))
    def rejected(name,call):
        try:call()
        except (EngineError,ValueError):passed(name)
        else:raise AssertionError('Expected explicit rejection: '+name)
    with Engine() as engine:
        library=Path(engine._lib._name).resolve()
        assert library.is_relative_to(Path(sys.prefix).resolve())
        library_hash=hashlib.sha256(library.read_bytes()).hexdigest()
        engine.call({'op':'configure_file_cache','bytes':1024**2})
        with engine.open_source(folder/fixture['sources']['original']['file'],id='source') as source:
            for name,geometry in fixture['geometries'].items():
                actual=source.measure(geometry,fixture['crs'],statistics=STATISTICS)
                for a,b in zip(actual['bands'],fixture['sources']['original']['expected'][name]):check_band(a,b)
                assert len(actual['bands'])==3
            passed('original-source-masks-nodata-scale-offset-signed-thin-outside',geometries=len(fixture['geometries']))
            selected=source.measure(fixture['geometries']['small'],fixture['crs'],bands=[2,0],statistics=STATISTICS)
            for a,b in zip(selected['bands'],[fixture['sources']['original']['expected']['small'][2],fixture['sources']['original']['expected']['small'][0]]):check_band(a,b)
            passed('requested-band-order')
            rejected('unknown-crs-rejected',lambda:source.measure(fixture['geometries']['small'],'EPSG:4326'))
            rejected('unknown-reducer-rejected',lambda:source.measure(fixture['geometries']['small'],fixture['crs'],statistics=['not-a-reducer']))
            rejected('bounded-quantile-overflow',lambda:source.measure(fixture['geometries']['whole'],fixture['crs'],statistics=['quantiles'],quantiles=[.5],quantile_max_samples=2))
            recovered=source.measure(fixture['geometries']['small'],fixture['crs'],bands=[0],statistics=STATISTICS)
            check_band(recovered['bands'][0],fixture['sources']['original']['expected']['small'][0])
            index_path=args.scratch/'summary-index'
            built=source.prepare(index_path,boundary_source='original',tile_edge=16)
            assert built['duplicate_raw_bytes']==0
            assert built['summary_bytes']==sum(p.stat().st_size for p in index_path.rglob('*') if p.is_file())
            with source.open_index(index_path/'summary.rsi',expected_build_id=built['build_id']) as index:
                indexed=index.measure(fixture['geometries']['small'],fixture['crs'],bands=[0],statistics=STATISTICS)
                assert indexed['bands']==recovered['bands']
                whole=index.measure(fixture['geometries']['whole'],fixture['crs'],statistics=STATISTICS)
                for a,b in zip(whole['bands'],fixture['sources']['original']['expected']['whole']):check_band(a,b)
                assert whole['work']['eligible_raw_interior_tiles_avoided']>0 and whole['io']['raw_reads']==0
                passed('summary-only-whole-interior-no-raw-reads',
                       eligible_tiles_avoided=whole['work']['eligible_raw_interior_tiles_avoided'],raw_reads=whole['io']['raw_reads'])
            rejected('closed-index-handle-rejected',lambda:index.measure(fixture['geometries']['small'],fixture['crs']))
            with engine.open_source(folder/fixture['sources']['mismatch']['file'],id='different') as mismatch:
                rejected('source-index-mismatch-rejected',lambda:mismatch.open_index(index_path/'summary.rsi'))
            with source.open_index(index_path/'summary.rsi') as index:
                with (index_path/'summary.rsi').open('ab') as output:output.write(b'changed-generated-index')
                rejected('retained-index-mutation-rejected',lambda:index.measure(fixture['geometries']['small'],fixture['crs']))
            passed('optional-original-source-index-reopened',index_bytes=built['summary_bytes'],duplicate_raw_bytes=0)
        rejected('closed-source-handle-rejected',lambda:source.inspect())
        with engine.open_source({'location':str(folder/fixture['sources']['original']['file']),'bands':[2,0]},id='mapped') as mapped:
            result=mapped.measure(fixture['geometries']['small'],fixture['crs'],statistics=STATISTICS)
            for target,original_band in enumerate((2,0)):
                check_band(result['bands'][target],fixture['sources']['original']['expected']['small'][original_band],target)
        passed('registered-source-band-mapping')
        mutation=args.scratch/'mutation.tif';shutil.copyfile(folder/fixture['sources']['original']['file'],mutation)
        with engine.open_source(mutation,id='mutation') as source:
            source.measure(fixture['geometries']['small'],fixture['crs'])
            with mutation.open('ab') as output:output.write(b'changed-generated-fixture')
            rejected('local-source-mutation-rejected',lambda:source.measure(fixture['geometries']['small'],fixture['crs']))
        job=make_job(folder,fixture)
        for mode in ('full','numeric'):
            job['output_mode']=mode;count=0;result_ids=set();last=None
            for page in engine.batch_pages(job,id='shared',max_rows=2):
                assert len(page['rows'])<=2
                if mode=='numeric':assert page['descriptor']['schema']=='skarve_numeric_six_v1'
                for row in page['rows']:
                    source_name,band=row['slice_id'].rsplit('-band',1)
                    check_band(row['bands'][0],fixture['sources'][source_name]['expected'][row['zone_id']][int(band)])
                    assert row['result_id'] not in result_ids;result_ids.add(row['result_id']);count+=1
                last=page
            assert count==18 and last['complete']
            metrics=last['metrics']
            assert metrics['source_opens']==2 and metrics['lazy_source_cache_hits']==4
            assert metrics['geometry_compilations']==3 and metrics['geometry_cache_hits']==15
            assert metrics['peak_tracked_bytes']<=job['budget']['working_bytes']
            passed('bounded-shared-scan-'+mode,rows=count,source_opens=metrics['source_opens'],geometry_compilations=metrics['geometry_compilations'])
        pages=engine.batch_pages(job,id='early',max_rows=2);next(pages);pages.close()
        rejected('early-iterator-close-releases-job',lambda:engine.call({'op':'job_info','id':'early'}))
        bad=copy.deepcopy(job);bad['budget']['output_bytes']=128
        rejected('batch-output-budget-rejected',lambda:list(engine.batch_pages(bad,id='bad',max_rows=2)))
        assert sum(len(p['rows']) for p in engine.batch_pages(job,id='recovered',max_rows=2))==18
        passed('batch-error-cleanup-and-reuse')

    with served(folder/fixture['remote']['file']) as server:
        with Engine() as engine:
            engine.call({'op':'configure_file_cache','bytes':1024**2})
            spec={'location':server.url,'http':{'allow_http':True,'cache_bytes':0,
                'max_requests':128,'max_range_bytes':1024**2,'max_download_bytes':8*1024**2}}
            with engine.open_source(spec,id='remote') as source:
                hot=[];evictions=0
                for name in ('hot-a','hot-b','hot-c','far-a','far-b','far-c','hot-a'):
                    before=len(server.snapshot())
                    result=source.measure(fixture['remote']['geometries'][name],fixture['crs'],statistics=STATISTICS)
                    check_band(result['bands'][0],fixture['remote']['expected'][name][0])
                    requests=server.snapshot()[before:]
                    hot.append({'query':name,'body_bytes':sum(r['body_bytes'] for r in requests),
                                'heads':sum(r['method']=='HEAD' for r in requests)})
                    evictions+=result['streaming']['cache_evictions']
                assert all(row['body_bytes']==0 and row['heads']>=2 for row in hot[1:3])
                assert evictions>0 and hot[-1]['body_bytes']>0
                assert engine.call({'op':'stats'})['decoded_cache_bytes']<=1024**2
                server.etag='"generated-next-version"'
                rejected('remote-source-validator-mutation-rejected',lambda:source.measure(fixture['remote']['geometries']['hot-a'],fixture['crs']))
                passed('remote-original-cog-owned-reader-hot-metadata-and-eviction',queries=hot,evictions=evictions)
    with Engine() as engine:
        for count in (36,40):
            values=np.array([1.25,-2,0,3.75],dtype=np.float32)
            windows=[{'bands':[{'id':100+b,'values':values} for b in range(count)]}]
            for policy,total,n in [('strict_selected_v1',3,4),('hm_demographics_ordered_v1',5,3)]:
                result=engine.bulk_reduce(windows,policy=policy,reducers=('sum','mean','min','max'))
                assert all(row['sum']==total and row['valid_count']==n and row['mean']==total/n for row in result['bands'])
            passed('typed-strict-ordered-'+str(count))
        values=np.array([1,np.nan,-2,-99,0,3,np.inf],dtype=np.float32)
        mask=np.array([0b11111010],dtype=np.uint8)
        win=[{'bands':[{'id':17,'values':values,'validity':mask,'validity_kind':'bits','validity_offset':1,'nodata':-99}]}]
        row=engine.bulk_reduce(win,policy='hm_demographics_ordered_v1')['bands'][0]
        assert row['sum']==4 and row['valid_count']==3
        assert all(row[key]==1 for key in ('excluded_mask','excluded_nodata','excluded_nonfinite','excluded_negative'))
        passed('typed-mask-and-exclusion-precedence')
        values=np.array([1,2,3],dtype=np.float64);win=[{'bands':[{'id':17,'values':values}]}]
        for options in ({'max_payload_bytes':4},{'max_contributions':1},{'policy':'unknown'},{'reducers':('sum','sum')}):
            rejected('typed-limit-or-contract-rejection',lambda options=options:engine.bulk_reduce(win,**options))
            assert skarve_bulk.reserved_snapshot_bytes()==0
        bad=copy.copy(win[0]);bad['selection']=np.array([9],dtype=np.uint32)
        rejected('typed-selection-overflow',lambda:engine.bulk_reduce([bad]))
        rejected('typed-arithmetic-overflow',lambda:engine.bulk_reduce([{'bands':[{'id':0,'values':np.array([1e308,1e308])}]}]))
        assert engine.bulk_reduce(win)['bands'][0]['sum']==6
        passed('typed-errors-release-reservations-and-reuse')
    async def cancellation():
        with Engine() as engine:
            values=np.ones(120000,dtype=np.float32)
            windows=[{'bands':[{'id':b,'values':values} for b in range(40)]}]
            pending=asyncio.create_task(engine.bulk_reduce_async(windows))
            await asyncio.sleep(0);pending.cancel()
            try:await pending
            except asyncio.CancelledError:pass
            else:raise AssertionError('Cancelled task returned success')
            assert skarve_bulk.reserved_snapshot_bytes()==0
            assert engine.bulk_reduce([{'bands':[{'id':0,'values':np.array([2],dtype=np.float32)}]}])['bands'][0]['sum']==2
        passed('async-cancellation-drains-and-allows-close')
    asyncio.run(cancellation())
    closed=Engine();closed.close();rejected('closed-engine-rejected',lambda:closed.call({'op':'stats'}))
    assert skarve_bulk.reserved_snapshot_bytes()==0
    print(json.dumps({'schema':1,'passed':True,'interface':'Python','checks':checks,'check_count':len(checks),
        'version':skarve.__version__,'library_sha256':library_hash,'network_scope':'generated loopback COG only',
        'new_external_network_requests':0,'owned_snapshot_bytes_after':0},allow_nan=False))


if __name__=='__main__':main()
