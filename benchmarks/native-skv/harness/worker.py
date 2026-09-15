#!/usr/bin/env python3
"""One installed source-cold job per bounded fresh process; no caller decoding."""
from __future__ import annotations
import argparse, contextlib, gzip, hashlib, json, os
from pathlib import Path
import resource, sys, time, traceback

for name in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS'):os.environ[name]='1'
os.environ['GDAL_CACHEMAX']='64';os.environ['PROJ_NETWORK']='OFF'
os.environ['GDAL_DISABLE_READDIR_ON_OPEN']='EMPTY_DIR'
os.environ['CPL_VSIL_CURL_ALLOWED_EXTENSIONS']='.tif,.skv,.rsi'
os.environ['GDAL_HTTP_MULTIRANGE']='SERIAL'
ROOT=Path(__file__).resolve().parent
sys.path.insert(0,str(ROOT))
from common import FIELDS, native_bands, upstream

def milliseconds(start):return (time.perf_counter()-start)*1000

def consume(rows):
    start=time.perf_counter();data=json.dumps(rows,sort_keys=True,separators=(',',':'),allow_nan=False).encode()
    return {'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'ms':milliseconds(start)}

def verify_installed_runtime(task,library):
    expected=task.get('expected_installed_runtime')
    if expected is None:return None
    assert library is None,'Installed worker must use its ordinary resolver'
    from raster_engine_lab import resolve_library
    start=time.perf_counter();resolved=Path(resolve_library()).resolve(strict=True)
    assert resolved==Path(expected['path']).resolve(strict=True),'Worker resolved a different installed runtime'
    with resolved.open('rb') as stream:sha=hashlib.file_digest(stream,'sha256').hexdigest()
    assert sha==expected['sha256'],'Worker installed runtime hash differs from freeze'
    return {'resolved_path':str(resolved),'sha256':sha,'verification_ms':milliseconds(start),
            'scope':'Default installed resolver immediately before engine creation; outside source timer, inside process wall. No injected library override.'}

def native_indexed(task,library):
    """Existing20-band index API, without raising its historical format cap."""
    from skarve import Skarve
    runtime_pin=verify_installed_runtime(task,library)
    start=time.perf_counter();engine=Skarve(library);created=milliseconds(start)
    with contextlib.ExitStack() as stack:
        stack.callback(engine.close);engine.call({'op':'configure_file_cache','bytes':task.get('decoded_cache_bytes',64<<20)})
        before_source=time.perf_counter();source_start_ns=time.monotonic_ns();groups=[]
        for i,descriptor in enumerate(task['index']['groups']):
            mapping=descriptor['source_bands'];selected=[mapping.index(b) for b in task['bands'] if b in mapping]
            if not selected:continue
            source=stack.enter_context(engine.infuse({**task['source'],'bands':mapping},id=f'group{i}'))
            groups.append({'id':f'group{i}','source':source,'mapping':mapping,'selected':selected,**descriptor})
        assert groups;opened=milliseconds(before_source);index_open=0;pages=0
        if len(groups)==1 and len(task['zones'])==1:
            group=groups[0];before_index=time.perf_counter();index=stack.enter_context(group['source'].open_index(group['location'],expected_build_id=group['build_id'],read_memory_bytes=128<<20));index_open=milliseconds(before_index)
            query=time.perf_counter();result=index.measure(task['zones'][0]['geometry'],task['crs'],bands=group['selected'],statistics=list(FIELDS))
            rows=[{'zone':task['zones'][0]['id'],'bands':native_bands(result,group['selected'])}]
            metrics={k:result[k] for k in ('work','streaming','source_access','timing_ms','persistent','io','transports') if k in result};provenance=result.get('provenance');pages=1
        else:
            query=time.perf_counter()
            job={'zones':task['zones'],'slices':[{'id':g['id'],'source':g['id'],'bands':g['selected'],'index':g['location'],'expected_build_id':g['build_id']} for g in groups],
                 'crs':task['crs'],'metrics':list(FIELDS),'backend':'native','schedule':'mixed','tile_edge':128,
                 'budget':{'working_bytes':1<<30,'tile_bytes':128<<20,'decoded_bytes':2<<30,'max_contributions':1_000_000_000,'workers':1}}
            assembled={z['id']:{} for z in task['zones']};seen=set();group_map={g['id']:g for g in groups};complete=False;metrics=None;provenance=None
            for page in engine.cleave(job,max_rows=128,checkpoint_interval=2**30):
                pages+=1;complete=page['complete'];metrics=page['metrics'];provenance=page.get('provenance',page.get('descriptor',{}).get('provenance'))
                for row in page['rows']:
                    key=(row['zone_id'],row['slice_id']);assert key not in seen;seen.add(key);group=group_map[row['slice_id']]
                    values=native_bands(row,group['selected'])
                    for local,value in zip(group['selected'],values):
                        original=group['mapping'][local];assert original not in assembled[row['zone_id']];assembled[row['zone_id']][original]=value
            assert complete and len(seen)==len(task['zones'])*len(groups)
            rows=[{'zone':zone['id'],'bands':[assembled[zone['id']][b] for b in task['bands']]} for zone in task['zones']]
        query_ms=milliseconds(query);sink=consume(rows);useful_ms=milliseconds(before_source);consumed_ns=time.monotonic_ns()
        close=time.perf_counter();stack.close();close_ms=milliseconds(close)
    return {'runtime_pin':runtime_pin,'source_to_consumed_ms':useful_ms,'source_start_monotonic_ns':source_start_ns,'consumed_monotonic_ns':consumed_ns,'engine_create_ms':created,'source_open_ms':opened,'index_open_ms':index_open,'query_ms':query_ms,'sink':sink,'close_ms':close_ms,'engine_lifecycle_ms':milliseconds(start),'answers':rows,'metrics':metrics,'provenance':provenance,'pages':pages,'index_reader_groups':len(groups),'index_contract':'Existing20-band source-bound index groups; one shared multi-slice job when multiple groups/zones, original global band order restored without changing numbers.'}

def native(task,library):
    if task.get('index'):return native_indexed(task,library)
    from skarve import Skarve
    runtime_pin=verify_installed_runtime(task,library)
    start=time.perf_counter();engine=Skarve(library);created=milliseconds(start)
    resources=contextlib.ExitStack();resources.callback(engine.close)
    try:
        engine.call({'op':'configure_file_cache','bytes':task.get('decoded_cache_bytes',64<<20)})
        before_source=time.perf_counter();source_start_ns=time.monotonic_ns()
        source=resources.enter_context(engine.infuse(task['source'],id='dataset'))
        opened=milliseconds(before_source);before_index=time.perf_counter();index=None
        if task.get('index') and len(task['zones'])==1:
            index=resources.enter_context(source.open_index(task['index']['location'],expected_build_id=task['index']['build_id']))
        index_open=milliseconds(before_index);query=time.perf_counter()
        options=dict(task.get('query_options',{}));backend=task['backend']
        if backend=='exactextract':options.update({'numerical_policy':'exactextract_fractional_v030','backend_options':{'strategy':task['strategy'],'window_bytes':256<<20,'max_cells_in_memory':262144}})
        if len(task['zones'])==1 and not task.get('force_single_cleave'):
            zone=task['zones'][0]
            if index:value=index.measure(zone['geometry'],task['crs'],bands=task['bands'],statistics=list(FIELDS),backend='native',**options)
            else:value=source.carve(zone['geometry'],crs=task['crs'],bands=task['bands'],metrics=list(FIELDS),backend=backend,**options)
            rows=[{'zone':zone['id'],'bands':native_bands(value,task['bands'])}]
            metrics={k:v for k,v in value.items() if k in ('streaming','work','source_access','timing_ms','persistent','io','transports')}
            provenance=value.get('provenance');pages=1
        else:
            slice_={'id':'raster','source':'dataset','bands':task['bands']}
            job={'zones':task['zones'],'slices':[slice_],'crs':task['crs'],'metrics':list(FIELDS),'backend':backend,
                 'budget':{'working_bytes':1<<30,'tile_bytes':256<<20,'decoded_bytes':2<<30,'max_contributions':1_000_000_000,'workers':1},**options}
            if backend=='native':
                job.update({'tile_edge':128,'window_policy':'source_layout'})
                if task.get('index'):
                    slice_.update({'index':task['index']['location'],'expected_build_id':task['index']['build_id']});job['schedule']='mixed'
            rows=[];pages=0;complete=False;metrics=None;provenance=None
            for page in engine.cleave(job,max_rows=128,checkpoint_interval=2**30):
                pages+=1;complete=page['complete'];metrics=page['metrics'];provenance=page.get('provenance',page.get('descriptor',{}).get('provenance'))
                rows.extend({'zone':row['zone_id'],'bands':native_bands(row,task['bands'])} for row in page['rows'])
            assert complete,'Incomplete batch result'
        query_ms=milliseconds(query);sink=consume(rows);useful_ms=milliseconds(before_source);consumed_ns=time.monotonic_ns()
        wanted=[z['id'] for z in task['zones']]
        assert len(rows)==len(wanted) and sorted(r['zone'] for r in rows)==sorted(wanted),'Missing/duplicate zone'
        close=time.perf_counter();resources.close();close_ms=milliseconds(close)
        return {'runtime_pin':runtime_pin,'source_to_consumed_ms':useful_ms,'source_start_monotonic_ns':source_start_ns,'consumed_monotonic_ns':consumed_ns,'engine_create_ms':created,'source_open_ms':opened,'index_open_ms':index_open,'query_ms':query_ms,'sink':sink,'close_ms':close_ms,'engine_lifecycle_ms':milliseconds(start),'answers':rows,'metrics':metrics,'provenance':provenance,'pages':pages}
    finally:resources.close()

def natural(task):
    import rasterio
    from exactextract.raster import RasterioRasterSource
    start=time.perf_counter()
    with rasterio.Env(GDAL_CACHEMAX=64<<20):
        before_source=time.perf_counter();source_start_ns=time.monotonic_ns()
        with rasterio.open(task['source']['location']) as dataset:
            assert all(dataset.scales[b]==1 and dataset.offsets[b]==0 for b in task['bands']),'Natural source control needs identity interpretation'
            rasters=[RasterioRasterSource(dataset,b+1,name=f'band{b}') for b in task['bands']]
            opened=milliseconds(before_source);query=time.perf_counter()
            rows=upstream(rasters,task['zones'],dataset.crs.wkt,task['strategy'],max_cells=262144)
            query_ms=milliseconds(query);sink=consume(rows);useful_ms=milliseconds(before_source);consumed_ns=time.monotonic_ns();close=time.perf_counter()
        close_ms=milliseconds(close)
    return {'source_to_consumed_ms':useful_ms,'source_start_monotonic_ns':source_start_ns,'consumed_monotonic_ns':consumed_ns,'source_open_ms':opened,'query_ms':query_ms,'sink':sink,'close_ms':close_ms,'engine_lifecycle_ms':milliseconds(start),'answers':rows,'policy':'exactextract_fractional_v030','source_integrity':'Natural Rasterio/upstream does not enforce Skarve trusted-manifest conditional identity contract; immutable loopback objects held stable.'}

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--task',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--library',type=Path);p.add_argument('--checkout-bindings',action='store_true');args=p.parse_args()
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    assert not args.checkout_bindings and args.library is None, 'Only the pinned ordinary installed resolver is allowed'
    task=json.loads(args.task.read_text());start=time.perf_counter();cpu=time.process_time();result={'task_id':task['id'],'passed':False}
    try:
        if not args.checkout_bindings and task['backend']!='upstream':
            assert task.get('expected_installed_runtime'),'Installed worker requires the frozen runtime identity'
        result.update(natural(task) if task['backend']=='upstream' else native(task,args.library));result['passed']=True
    except Exception as error:result['error']={'type':type(error).__name__,'message':str(error)}
    result.update({'worker_ms':milliseconds(start),'cpu_seconds':time.process_time()-cpu,'peak_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,'address_space_limit_bytes':2<<30})
    assert not args.output.exists(),'No overwrite'
    with gzip.open(args.output, 'xt', encoding='utf-8') as stream:
        stream.write(json.dumps(result,separators=(',',':'),allow_nan=False)+'\n')
    print(json.dumps({'task':task['id'],'passed':result['passed'],'ms':result.get('source_to_consumed_ms'),'error':result.get('error')}),flush=True)
    return 0 if result['passed'] else 1

if __name__=='__main__':raise SystemExit(main())
