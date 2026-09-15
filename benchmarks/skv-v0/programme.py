#!/usr/bin/env python3
"""Freeze and execute a bounded complete-operation cold-format comparison.

Prepared manifest sources contain id, logical, directory and variants. Each
variant provides path/sha256/bytes; COG variants may provide an index descriptor
{path,build_id}. SKV summary/raw lanes use exactly the same immutable object.
"""
from __future__ import annotations
import argparse, hashlib, json, random, subprocess, sys, time
from collections import Counter
from pathlib import Path
from cohort import workload
from range_server import served

ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'benchmarks/beta2'))
from common import differences

REGIMES={'local':None,'http0':(0,0),'http8':(8,64),'http25':(25,16)}
LANES={
    'native-ordinary':('ordinary','native',None),
    'native-cog128':('cog128','native',None),
    'native-cog256':('cog256','native',None),
    'native-cog256-single-cleave':('cog256','native','single-cleave'),
    'native-ordinary-single-cleave':('ordinary','native','single-cleave'),
    'native-band256':('band256','native',None),
    'native-cog-index':('cog256','native','index256'),
    'native-cog-index64':('cog256','native','index64'),
    'native-cogband128':('cogband128','native',None),
    'native-cogtile128':('cogtile128','native',None),
    'native-cogband-index':('cogband128','native','index256'),
    'native-cogband-index64':('cogband128','native','index64'),
    'native-cogtile-index':('cogtile128','native','index256'),
    'native-cogtile-index64':('cogtile128','native','index64'),
    'native-skv-raw':('skv','native','raw'),
    'native-skv-summary':('skv','native','summary'),
    'ee-ordinary-feature':('ordinary','exactextract','feature-sequential'),
    'ee-ordinary-raster':('ordinary','exactextract','raster-sequential'),
    'ee-cog-feature':('cog256','exactextract','feature-sequential'),
    'ee-cog-raster':('cog256','exactextract','raster-sequential'),
    'ee-skv-feature':('skv','exactextract','feature-sequential'),
    'ee-skv-raster':('skv','exactextract','raster-sequential'),
    'ee-cogtile-feature':('cogtile128','exactextract','feature-sequential'),
    'ee-cogtile-raster':('cogtile128','exactextract','raster-sequential'),
    'upstream-ordinary-feature':('ordinary','upstream','feature-sequential'),
    'upstream-ordinary-raster':('ordinary','upstream','raster-sequential'),
    'upstream-cog-feature':('cog256','upstream','feature-sequential'),
    'upstream-cog-raster':('cog256','upstream','raster-sequential'),
    'upstream-cog128-feature':('cog128','upstream','feature-sequential'),
    'upstream-cog128-raster':('cog128','upstream','raster-sequential'),
    'upstream-cogband-feature':('cogband128','upstream','feature-sequential'),
    'upstream-cogband-raster':('cogband128','upstream','raster-sequential'),
    'upstream-cogtile-feature':('cogtile128','upstream','feature-sequential'),
    'upstream-cogtile-raster':('cogtile128','upstream','raster-sequential'),
}

def digest(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def write(path,value):
    assert not path.exists(),'Use a fresh evidence destination';path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value,indent=2,allow_nan=False)+'\n')

def freeze(args):
    installed_bindings={}
    environment=json.loads(subprocess.check_output([str(args.python),'-c','import sys,json,platform,rasterio; from importlib.metadata import version; print(json.dumps({"python":sys.version,"machine":platform.machine(),"system":platform.system(),"kernel":platform.release(),"packages":{name:version(name) for name in ("numpy","rasterio","exactextract")},"natural_reader_gdal":rasterio.__gdal_version__}))'],text=True))
    if not args.checkout_bindings:
        resolved=subprocess.check_output([str(args.python),'-c','from raster_engine_lab import resolve_library; print(resolve_library())'],text=True).strip()
        assert Path(resolved).resolve()==args.library.resolve(),'Final timing must use the installed package native library'
        paths=json.loads(subprocess.check_output([str(args.python),'-c','import json,skarve,raster_engine_lab; from pathlib import Path; print(json.dumps([str(p) for m in (skarve,raster_engine_lab) for p in Path(m.__file__).parent.rglob("*.py")]))'],text=True))
        installed_bindings={path:digest(path) for path in sorted(paths)}
    prepared=json.loads(args.prepared.read_text());selected=set(args.datasets.split(',')) if args.datasets else None;tasks=[]
    source_manifest={}
    for source in prepared['sources']:
        if selected and source['id'] not in selected:continue
        assert source['id'] not in source_manifest,'Duplicate dataset identity'
        source_manifest[source['id']]=source
        query_grid=source['logical'];query_window=None
        if source['id']=='worldpop1' and query_grid['width']>2048 and not args.whole_source_geometry:
            transform=list(query_grid['transform']);x=int((-1.52-transform[0])/transform[1])-512;y=int((12.37-transform[3])/transform[5])-512
            query_window=[x,y,1024,1024];transform[0]+=x*transform[1];transform[3]+=y*transform[5]
            query_grid={**query_grid,'width':1024,'height':1024,'transform':transform}
        for case in args.cases:
            if case in ('C','D') and len(source['logical']['bands'])==1:continue
            for repeat in range(args.repeats):
                for regime in args.regimes.split(','):
                    assert regime in REGIMES
                    for lane in args.lanes.split(','):
                        assert lane in LANES
                        variant,backend,mode=LANES[lane]
                        if mode=='single-cleave' and case not in ('A','C'):continue
                        assert variant in source['variants'],(source['id'],variant,'Required format absent')
                        if mode and mode.startswith('index'):assert source['variants'][variant].get(mode),'COG summary control absent'
                        bands=([min(23,len(source['logical']['bands'])-1)] if case in ('A','B') else list(range(len(source['logical']['bands']))))
                        task={'id':f'{source["id"]}-{case}-{repeat}-{regime}-{lane}', 'dataset':source['id'],'case':case,'round':repeat,'regime':regime,'lane':lane,'variant':variant,'backend':backend,'mode':mode,'bands':bands,
                              'zones':workload(query_grid,case,seed=args.seed+repeat*997,single_family=args.single_family,many=args.polygons),'crs':source['crs'],'query_window_xywh':query_window,'source_extent':'Complete original raster; query window does not crop serving object.'}
                        tasks.append(task)
    assert source_manifest and tasks
    random.Random(args.seed+31).shuffle(tasks)
    paths=list(Path(__file__).parent.glob('*.py'))+[ROOT/'benchmarks/beta2/common.py']
    if args.checkout_bindings:paths+=list((ROOT/'bindings/python').rglob('*.py'))
    code={str(p.relative_to(ROOT)):digest(p) for p in sorted(set(paths))}
    result={'schema':'skarve_skv_frozen_cold_programme_v1','seed':args.seed,'sources':source_manifest,'tasks':tasks,'code_sha256':code,'installed_bindings_sha256':installed_bindings,'prepared_sha256':digest(args.prepared),'library_sha256':digest(args.library),'library':str(args.library),'bindings_mode':'checkout-development' if args.checkout_bindings else 'installed','timer':'Before source opening through complete consumed five-field result; process wall includes interpreter/import/startup/exit. Cold reader caches, uncontrolled OS cache.','limits':{'process_address_space_bytes':2<<30,'working_bytes':1<<30,'decoder_cache_bytes':64<<20,'gdal_cache_bytes':64<<20,'workers':1,'http_source_per_registered_reader':{'max_requests':8192,'max_download_bytes':768<<20,'max_range_bytes':4<<20,'transfer_cache_bytes':4<<20},'legacy_index_per_reader':{'max_requests':4096,'max_download_bytes':128<<20,'configured_transfer_cache_bytes':0},'loopback_combined_per_job':{'max_requests':8192,'max_download_bytes':1<<30},'exactextract':{'max_cells_in_memory':262144,'window_bytes':256<<20},'source_index_groups':'One or two registered source/index pairs as required by selected original bands. Transport allowances are per reader; loopback ceiling and process memory cap apply to the complete job.'},'regimes':REGIMES}
    if any(task['backend']=='native' for task in tasks):
        assert args.native_reference in args.lanes.split(',') and LANES[args.native_reference][1]=='native','Declared numerical reference lane absent'
    result['native_reference']=args.native_reference
    result['exactextract_references']={'feature-sequential':args.ee_feature_reference,'raster-sequential':args.ee_raster_reference}
    for strategy,reference in result['exactextract_references'].items():
        if any(task['backend'] in ('exactextract','upstream') and task['mode']==strategy for task in tasks):
            assert reference in args.lanes.split(',') and LANES[reference][1] in ('exactextract','upstream') and LANES[reference][2]==strategy,'Matching-policy declared exactextract reference absent'
    result['python_environment']=environment
    result['runtime_dependencies']={str(path.resolve()):digest(path.resolve()) for path in args.runtime_dependency}
    result['limits']['native_index_read_bytes']=128<<20
    result['limits']['native_batch_tile_bytes']=256<<20
    result['limits']['native_index_batch_tile_bytes']=128<<20
    result['limits']['native_single_carve_read_bytes']=64<<20
    result['installed_runtime_verification']='Re-resolve and verify the default installed native path and SHA256 at programme run and immediately before each native worker engine construction. Per-worker verification is outside source-to-consumed time, inside process wall time, and recorded; no development-library override is injected.'
    write(args.output/'freeze.json',result);print(json.dumps({'tasks':len(tasks),'freeze':str(args.output/'freeze.json')}))

def location_task(spec,source,regime,server=None):
    variant=source['variants'][spec['variant']];directory=Path(source['directory']);path=directory/variant['path'];remote=server is not None
    source_spec={'location':server.url('source'+path.suffix) if remote else str(path),
        'identity':{'sha256':variant['sha256'],'byte_length':variant['bytes'],'policy':'trusted_manifest'}}
    if remote:source_spec.update({'http':{'allow_http':True,'max_requests':8192,'max_download_bytes':768<<20,'max_range_bytes':4<<20,'cache_bytes':4<<20}});source_spec['identity']['etag']='"'+variant['sha256']+'"'
    if spec['mode']=='raw':source_spec['use_summaries']=False
    if spec['variant']=='skv' and spec['backend']=='exactextract':source_spec['use_summaries']=False
    task={**spec,'source':source_spec,'decoded_cache_bytes':64<<20}
    if spec['mode']=='single-cleave':task['force_single_cleave']=True
    if spec['backend'] in ('exactextract','upstream'):task['strategy']=spec['mode']
    if spec['mode'] and spec['mode'].startswith('index'):
        groups=[]
        for i,index in enumerate(variant[spec['mode']]['groups']):
            if not set(index['source_bands']).intersection(spec['bands']):continue
            groups.append({'source_bands':index['source_bands'],'location':server.url(f'summary-{i:02d}.rsi') if remote else str(directory/index['path']),'build_id':index['build_id']})
        task['index']={'groups':groups}
    return task

def run(args):
    frozen=json.loads((args.output/'freeze.json').read_text());assert digest(args.library)==frozen['library_sha256']
    assert frozen['bindings_mode']==('checkout-development' if args.checkout_bindings else 'installed'),'Binding scope differs from frozen programme'
    if not args.checkout_bindings:
        resolved=subprocess.check_output([str(args.python),'-c','from raster_engine_lab import resolve_library; print(resolve_library())'],text=True).strip()
        assert Path(resolved).resolve()==Path(frozen['library']).resolve(),'Installed runtime path differs from freeze under the execution environment'
        assert digest(resolved)==frozen['library_sha256'],'Resolved installed runtime changed after freeze'
    assert all(digest(ROOT/path)==sha for path,sha in frozen['code_sha256'].items()),'Harness changed after freeze'
    assert all(digest(path)==sha for path,sha in frozen.get('installed_bindings_sha256',{}).items()),'Installed bindings changed after freeze'
    assert all(digest(path)==sha for path,sha in frozen.get('runtime_dependencies',{}).items()),'Qualified runtime dependency changed after freeze'
    results=[];started=time.perf_counter();setup=time.perf_counter();verified={};setup_bytes=0
    for source in frozen['sources'].values():
        for variant in source['variants'].values():
            for item in [variant]+[group for key,index in variant.items() if key.startswith('index') and isinstance(index,dict) for group in index['groups']]:
                path=(Path(source['directory'])/item['path']).resolve()
                if str(path) in verified:continue
                stat=path.stat();assert stat.st_size==item['bytes'] and digest(path)==item['sha256'],'Frozen source/index changed'
                verified[str(path)]={'sha256':item['sha256'],'generation':(stat.st_dev,stat.st_ino,stat.st_size,stat.st_mtime_ns)};setup_bytes+=stat.st_size
    setup_receipt={'wall_ms':(time.perf_counter()-setup)*1000,'bytes_hashed':setup_bytes,'objects':len(verified),'scope':'Once-per-programme fixture/index integrity setup, outside individual query timings. Reads can warm OS cache; source-cold is not disk-cold. Per-query engine source verification remains inside the primary timer.'}
    for ordinal,spec in enumerate(frozen['tasks']):
        source=frozen['sources'][spec['dataset']];variant=source['variants'][spec['variant']];directory=Path(source['directory']);path=directory/variant['path']
        stat=path.stat();assert (stat.st_dev,stat.st_ino,stat.st_size,stat.st_mtime_ns)==verified[str(path.resolve())]['generation'],'Frozen object generation changed'
        task_path=args.output/'tasks'/f'{ordinal:05d}.json';out=args.output/'tasks'/f'{ordinal:05d}-result.json'
        objects={'source'+path.suffix:path}
        if spec['mode'] and spec['mode'].startswith('index'):
            for i,index in enumerate(variant[spec['mode']]['groups']):
                if set(index['source_bands']).intersection(spec['bands']):objects[f'summary-{i:02d}.rsi']=directory/index['path']
        for object_path in objects.values():
            stat=object_path.stat();assert (stat.st_dev,stat.st_ino,stat.st_size,stat.st_mtime_ns)==verified[str(object_path.resolve())]['generation']
        server_context=served(objects,delay_ms=REGIMES[spec['regime']][0],mib_s=REGIMES[spec['regime']][1],verified_sha256={key:verified[str(p.resolve())]['sha256'] for key,p in objects.items()}) if REGIMES[spec['regime']] else None
        def execute(server):
            task=location_task(spec,source,spec['regime'],server)
            if not args.checkout_bindings:
                task['expected_installed_runtime']={'path':frozen['library'],'sha256':frozen['library_sha256']}
            write(task_path,task)
            command=[str(args.python),str(Path(__file__).with_name('worker.py')),'--task',str(task_path),'--output',str(out)]
            if args.checkout_bindings:command+=['--checkout-bindings','--library',str(args.library)]
            start=time.perf_counter()
            try:
                process=subprocess.run(command,capture_output=True,text=True,timeout=args.timeout)
                record=json.loads(out.read_text()) if out.exists() else {'passed':False,'error':{'message':'No worker receipt'}}
                record['exit_code']=process.returncode;record['passed'] &= process.returncode==0
                record['process_output']={'stdout_tail':process.stdout[-8192:],'stderr_tail':process.stderr[-8192:],'tail_character_bound':8192}
            except subprocess.TimeoutExpired:record={'passed':False,'error':{'message':'Bounded worker timeout'}}
            record['process_wall_ms']=(time.perf_counter()-start)*1000
            if server:
                timeline=server.snapshot()
                for request in timeline:
                    begin=record.get('source_start_monotonic_ns');end=record.get('consumed_monotonic_ns')
                    request['phase']='source_to_consumed' if begin is not None and end is not None and begin<=request['start_monotonic_ns']<=end else 'outside_primary_or_failed_attempt'
                primary=[r for r in timeline if r['phase']=='source_to_consumed']
                record['http']={'requests':len(timeline),'body_bytes':sum(r['body_bytes'] for r in timeline),'head_requests':sum(r['method']=='HEAD' for r in timeline),'get_requests':sum(r['method']=='GET' for r in timeline),'primary_requests':len(primary),'primary_body_bytes':sum(r['body_bytes'] for r in primary),'timeline':timeline}
            return record
        if server_context:
            with server_context as server:record=execute(server)
        else:record=execute(None)
        record['task']=spec;results.append(record);write(args.output/'records'/f'{ordinal:05d}.json',record)
        print(json.dumps({'task':spec['id'],'passed':record['passed'],'ms':record.get('source_to_consumed_ms'),'bytes':record.get('http',{}).get('body_bytes'),'error':record.get('error')}),flush=True)
    assert Counter(r['task']['id'] for r in results)==Counter(t['id'] for t in frozen['tasks'])
    validation=[]
    lookup={(r['task']['dataset'],r['task']['case'],r['task']['round'],r['task']['regime'],r['task']['lane']):r for r in results}
    for record in results:
        task=record['task']
        if not record['passed']:continue
        lane=frozen.get('native_reference','native-ordinary') if task['backend']=='native' else frozen.get('exactextract_references',{'feature-sequential':'ee-ordinary-feature','raster-sequential':'ee-ordinary-raster'})[task['mode']]
        key=(task['dataset'],task['case'],task['round'],task['regime'],lane);control=lookup.get(key)
        if not control or not control['passed']:
            validation.append({'task':task['id'],'passed':False,'reason':'Matching declared control absent/failed'});continue
        expected={row['zone']:row['bands'] for row in control['answers']};errors=[]
        for row in record['answers']:errors.extend({'zone':row['zone'],**error} for error in differences(row['bands'],expected[row['zone']],strict=task['backend']=='native'))
        validation.append({'task':task['id'],'control':control['task']['id'],'passed':not errors,'checks':len(task['bands'])*len(task['zones'])*5,'differences':errors})
    result={'schema':'skarve_skv_cold_results_v1','passed':all(r['passed'] for r in results) and all(r['passed'] for r in validation),'fixture_integrity_setup':setup_receipt,'freeze_sha256':digest(args.output/'freeze.json'),'records':results,'format_validation':validation,'total_seconds':time.perf_counter()-started}
    write(args.output/'programme.json',result)
    print(json.dumps({'passed':result['passed'],'tasks':len(results),'failed_execution':sum(not r['passed'] for r in results),'failed_validation':sum(not r['passed'] for r in validation)}))

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('stage',choices=['freeze','run']);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--prepared',type=Path);p.add_argument('--library',type=Path,required=True)
    p.add_argument('--python',type=Path,default=Path(sys.executable));p.add_argument('--checkout-bindings',action='store_true')
    p.add_argument('--datasets');p.add_argument('--cases',default='ABCD');p.add_argument('--regimes',default='local,http0')
    p.add_argument('--lanes',default=','.join(lane for lane in LANES if 'single-cleave' not in lane))
    p.add_argument('--native-reference',default='native-ordinary')
    p.add_argument('--runtime-dependency',type=Path,action='append',default=[],help='Pin a qualified resolved dynamic native dependency.')
    p.add_argument('--ee-feature-reference',default='ee-ordinary-feature')
    p.add_argument('--ee-raster-reference',default='ee-ordinary-raster')
    p.add_argument('--repeats',type=int,default=1);p.add_argument('--seed',type=int,default=441991)
    p.add_argument('--single-family',default='interior');p.add_argument('--polygons',type=int,default=8)
    p.add_argument('--timeout',type=int,default=180);p.add_argument('--whole-source-geometry',action='store_true');args=p.parse_args()
    return freeze(args) if args.stage=='freeze' else run(args)

if __name__=='__main__':main()
