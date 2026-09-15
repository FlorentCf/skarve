#!/usr/bin/env python3
"""Portable, separately identified replication of the frozen native programme.

No data downloads, source conversion, route search or retries. Preparation is explicit.
The measured d15 observations remain unchanged. New runtime/object hashes are recorded.
"""
import argparse,copy,fcntl,gzip,hashlib,importlib.util,json,os,resource,subprocess,sys,time
from pathlib import Path
HERE=Path(__file__).resolve().parent
def harness_module(name):
    # Avoid generic module names (common/protocol) colliding with other test tools.
    spec=importlib.util.spec_from_file_location('skarve_replication_'+name,HERE/'harness'/f'{name}.py')
    module=importlib.util.module_from_spec(spec)
    sys.modules[spec.name]=module
    spec.loader.exec_module(module)
    return module

protocol=harness_module('protocol')
LIMITS=protocol.LIMITS
expand_task=protocol.expand_task
validate_answers=protocol.validate_answers
validate_execution=protocol.validate_execution
differences=harness_module('common').differences
served=harness_module('range_server').served

def require(condition,message):
    if not condition:raise ValueError(message)

def python_invocation(path):
    # A venv interpreter is normally a symlink. Resolving it loses that venv.
    return os.path.abspath(os.fspath(path))

def select_tasks(original,wanted,smoke):
    require(wanted and wanted<=set(original['sources']),'Unknown dataset family')
    tasks=[t for t in original['tasks'] if t['dataset'] in wanted]
    if smoke:
        tasks=[t for t in tasks if t['case']=='A' and t['round']==0 and t['regime']=='local' and t['lane'] in ['native-cog','native-skv-summary']]
    return tasks

def generation(path):
    s=Path(path).stat()
    return (s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns)

def probe_runtime(python,natural):
    probe=r"""
import hashlib,importlib.metadata,json,platform,sys
from pathlib import Path
import raster_engine_lab,skarve
from raster_engine_lab import resolve_library

def digest(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()
library=Path(resolve_library()).resolve()
bindings={str(Path(m.__file__).resolve()):digest(m.__file__) for m in [raster_engine_lab,skarve]}
versions={'python':sys.version,'skarve':skarve.__version__,'platform':platform.platform()}
if sys.argv[1]=='natural':
    import rasterio
    versions.update({name:importlib.metadata.version(name) for name in ['exactextract','numpy','rasterio']})
    versions['rasterio_gdal']=rasterio.__gdal_version__
print(json.dumps({'library':str(library),'library_sha256':digest(library),'bindings_sha256':bindings,
                 'actual_versions':versions,'python_executable_sha256':digest(sys.executable)}))
"""
    result=json.loads(subprocess.check_output([python_invocation(python),'-c',probe,'natural' if natural else 'native'],env=env(),text=True))
    result['python']=python_invocation(python)
    return result

def sha(p):
    with Path(p).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()
def read(p):
    with (gzip.open(p,'rt') if str(p).endswith('.gz') else open(p)) as stream:return json.load(stream)
def write(p,value):
    p=Path(p);p.parent.mkdir(parents=True,exist_ok=True)
    with p.open('xb') as stream:
        data=(json.dumps(value,sort_keys=True,allow_nan=False)+'\n').encode()
        if p.suffix=='.gz':
            with gzip.GzipFile(fileobj=stream,mode='wb',mtime=0,filename='') as z:z.write(data)
        else:stream.write(data)
def env():
    result=os.environ.copy()
    for k in ['SKARVE_LIBRARY','RASTER_ENGINE_LIBRARY','RASTER_ENGINE_LIB','PYTHONPATH']:result.pop(k,None)
    for k in ['OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS']:result[k]='1'
    result.update(GDAL_CACHEMAX='64',PROJ_NETWORK='OFF',GDAL_DISABLE_READDIR_ON_OPEN='EMPTY_DIR',CPL_VSIL_CURL_ALLOWED_EXTENSIONS='.tif,.skv,.rsi',GDAL_HTTP_MULTIRANGE='SERIAL',PYTHONDONTWRITEBYTECODE='1')
    return result

def validate_index(actual,expected):
    require(actual['tile_edge']==expected['tile_edge'],'Changed index tile edge')
    require([g['source_bands'] for g in actual['groups']]==[g['source_bands'] for g in expected['groups']],
            'Changed index group mapping or reduction order')

def freeze(args):
    original=read(HERE/'FROZEN_PROGRAMME.json');wanted=set(args.datasets.split(','));sources={}
    protocol.validate_tasks(original)
    tasks=select_tasks(original,wanted,args.smoke)
    for path in args.prepared:
        for source in read(path)['sources']:
            if source['id'] in wanted:
                require(source['id'] not in sources,'Duplicate dataset')
                source=copy.deepcopy(source);directory=Path(source['directory']).resolve()
                require(source['logical']==original['sources'][source['id']]['logical'],'Logical pixels/masks/interpretation differ from declared dataset')
                source['directory']=str(directory);sources[source['id']]=source
    require(set(sources)==wanted,'Missing prepared dataset')
    objects={}
    for task in tasks:
        source=sources[task['dataset']];v=source['variants'][task['variant']];directory=Path(source['directory'])
        expected=original['sources'][task['dataset']]['variants'][task['variant']]
        for key in ['block','creation_options','image_structure']:
            if key in expected:require(v.get(key)==expected[key],f'Changed layout {task["dataset"]}/{task["variant"]}/{key}')
        checks=[v]
        if task['mode'] in ['index64','index256']:
            validate_index(v[task['mode']],expected[task['mode']])
            checks+=v[task['mode']]['groups']
        for item in checks:
            path=(directory/item['path']).resolve();identity={'sha256':item['sha256'],'bytes':item['bytes']}
            if str(path) in objects:require(objects[str(path)]==identity,'Conflicting prepared object identity')
            else:
                require(path.is_file() and path.stat().st_size==item['bytes'] and sha(path)==item['sha256'],'Prepared object changed')
                objects[str(path)]=identity
        if task['variant']=='skv':
            import struct,zlib
            with (directory/v['path']).open('rb') as stream:header=stream.read(16384)
            size=struct.unpack_from('<I',header,24)[0];metadata=json.loads(zlib.decompress(header[64:64+size]))
            require(metadata['chunk_edge']==128 and metadata['band_group']==len(source['logical']['bands']),'Changed SKV chunk/band-group recipe')
            require(metadata['codec']=='deflate' and metadata.get('predictor')=='byte_delta_v1' and metadata.get('payload_layout','band')=='band' and metadata['summaries'] is True,'Changed SKV recipe')
    natural=any(t['backend']=='upstream' for t in tasks)
    runtime=probe_runtime(args.python,natural)
    if natural:
        for key in ['exactextract','numpy','rasterio','rasterio_gdal']:
            require(runtime['actual_versions'][key]==original['runtime']['actual_versions'][key],f'Changed frozen natural-control dependency: {key}')
    runtime['baseline_library_sha256']=original['runtime']['library_sha256']
    runtime['same_library_bytes_as_historical']=runtime['library_sha256']==runtime['baseline_library_sha256']
    frozen={**original,'schema':'skarve_public_native_replication_v1','runtime':runtime,'sources':sources,'tasks':tasks,'objects':objects,
            'counts':{'operations':len(tasks),'smoke_only':args.smoke},'historical_programme_sha256':sha(HERE/'FROZEN_PROGRAMME.json'),
            'historical_source_commit':original['source_commit'],'source_commit':None,'replication_note':'New execution identity; never replaces original d15 observations. Same logical data, frozen geometry/control strategies and limits. Changed preparation/runtime bytes are explicitly pinned.',
            'input_hashes':{str(path.resolve()):sha(path) for path in args.prepared},
            'harness_hashes':{str(path.resolve()):sha(path) for path in sorted((HERE/'harness').glob('*.py'))},'driver_sha256':sha(Path(__file__))}
    args.output.mkdir(parents=True,exist_ok=False);write(args.output/'freeze.json',frozen)
    print(json.dumps({'operations':len(tasks),'freeze_sha256':sha(args.output/'freeze.json'),'same_library_as_d15':runtime['same_library_bytes_as_historical'],'smoke_only':args.smoke}))

def validate_results(f,results,root):
    lookup={r['task_id']:r for r in results};validation=[]
    for task in f['tasks']:
        record=lookup.get(task['id'])
        check={'task':task['id'],'attempted':record is not None,'passed':bool(record and record['passed']),'differences':[]}
        if record is None:check['reason']='Programme stopped before this operation; retained in denominator'
        elif not record['passed']:check['reason']='Operation failed; inspect immutable operation record'
        elif task['backend']=='native':
            refs=[t for t in f['tasks'] if all(t[k]==task[k] for k in ['dataset','case','round','regime']) and t['lane']=='native-cog']
            require(len(refs)==1,'Missing or ambiguous frozen native control')
            ref=lookup.get(refs[0]['id'])
            if not ref or not ref['passed']:check.update(passed=False,reason='Native COG control unavailable')
            else:
                a=read(root/record['worker_file']);b=read(root/ref['worker_file'])
                for x,y in zip(a['answers'],b['answers']):check['differences'].extend(differences(x['bands'],y['bands'],strict=True))
                check['passed']=not check['differences']
        validation.append(check)
    return validation

def run(args):
    root=args.output
    require(sha(root/'freeze.json')==args.freeze_sha256,'Wrong frozen programme')
    f=read(root/'freeze.json')
    require(f['schema']=='skarve_public_native_replication_v1','Wrong replication schema')
    with (root/'execution.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        require(not (root/'attempts').exists() and not (root/'complete.json').exists(),
                'No retries or replacement of an attempted programme; use a new explicit replication')
        for path,digest in {**f['harness_hashes'],**f['input_hashes']}.items():require(sha(path)==digest,'Frozen code/receipt changed')
        require(sha(__file__)==f['driver_sha256'],'Frozen driver changed')
        natural=any(t['backend']=='upstream' for t in f['tasks'])
        actual=probe_runtime(f['runtime']['python'],natural)
        for key,value in actual.items():require(f['runtime'][key]==value,f'Frozen installed runtime changed: {key}')
        generations={}
        for path,item in f['objects'].items():
            require(Path(path).stat().st_size==item['bytes'] and sha(path)==item['sha256'],'Frozen source object changed')
            generations[path]=generation(path)
        # Keep only pointers in the coordinator: full HTTP traces live on disk.
        results=[];stop_reason=None
        for ordinal,spec in enumerate(f['tasks']):
            if sum(p.stat().st_size for p in root.rglob('*') if p.is_file())>=128<<20:
                stop_reason='Evidence cap reached; completed prefix retained'
                break
            write(root/'attempts'/f'{ordinal:05d}.json',{'task':spec['id'],'ordinal':ordinal})
            output=root/'workers'/f'{ordinal:05d}.json.gz';output.parent.mkdir(exist_ok=True)
            record={'task_id':spec['id'],'passed':False};objects={}
            def action(server):
                task=expand_task(f,spec,server);task_path=root/'tasks'/f'{ordinal:05d}.json';write(task_path,task)
                before=time.perf_counter();cpu=resource.getrusage(resource.RUSAGE_CHILDREN)
                try:
                    result=subprocess.run([f['runtime']['python'],str(HERE/'harness/worker.py'),'--task',str(task_path),'--output',str(output)],env=env(),capture_output=True,text=True,timeout=LIMITS['timeout_seconds'])
                    record.update(exit_code=result.returncode,stderr=result.stderr[-8192:])
                    if output.exists():
                        worker=read(output);record['passed']=result.returncode==0 and worker.get('passed') is True
                        record.update(worker_file=str(output.relative_to(root)),worker_sha256=sha(output))
                        if record['passed']:validate_answers(worker,task);validate_execution(worker,task)
                        for key in ['source_start_monotonic_ns','consumed_monotonic_ns']:record[key]=worker.get(key)
                    else:record['error']={'type':'MissingWorkerResult','message':'Worker produced no result'}
                finally:
                    after=resource.getrusage(resource.RUSAGE_CHILDREN)
                    record.update(process_wall_ms=(time.perf_counter()-before)*1000,process_cpu_seconds=after.ru_utime+after.ru_stime-cpu.ru_utime-cpu.ru_stime)
            try:
                source=f['sources'][spec['dataset']];variant=source['variants'][spec['variant']];directory=Path(source['directory'])
                path=(directory/variant['path']).resolve();objects={'source'+path.suffix:path}
                if spec['mode'] in ['index64','index256']:
                    for i,index in enumerate(variant[spec['mode']]['groups']):
                        if set(index['source_bands'])&set(spec['bands']):objects[f'summary-{i:02d}.rsi']=(directory/index['path']).resolve()
                for path in objects.values():require(generation(path)==generations[str(path)],'Source changed after freeze')
                if spec['regime']=='http0':
                    with served(objects,delay_ms=0,mib_s=0,verified_sha256={k:f['objects'][str(p)]['sha256'] for k,p in objects.items()}) as server:
                        try:action(server)
                        finally:record['http']=server.snapshot()
                else:action(None)
            except Exception as error:
                record.update(passed=False,error={'type':type(error).__name__,'message':str(error)})
            # Verification lies outside primary timing, for every lane including upstream.
            # A mutation invalidates this result and stops the remaining immutable schedule.
            try:
                for path in objects.values():require(generation(path)==generations[str(path)],'Source changed during programme')
            except Exception as error:
                stop_reason=str(error);record.update(passed=False,source_integrity_error=stop_reason)
            record_file=root/'records'/f'{ordinal:05d}.json.gz';write(record_file,record)
            results.append({key:record[key] for key in ['task_id','passed','worker_file'] if key in record})
            print(json.dumps({'ordinal':ordinal,'task':spec['id'],'passed':record['passed']}),flush=True)
            if stop_reason:break
        validation=validate_results(f,results,root)
        write(root/'complete.json',{'schema':'skarve_public_replication_results_v1','freeze_sha256':args.freeze_sha256,
              'planned':len(f['tasks']),'attempted':len(results),'unattempted':len(f['tasks'])-len(results),
              'passed':sum(r['passed'] for r in validation),'all_failures_in_denominator':True,
              'programme_completed':stop_reason is None,'stop_reason':stop_reason,'validation':validation,'new_runtime':f['runtime']})

def main():
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='stage',required=True)
    a=sub.add_parser('freeze');a.add_argument('--prepared',type=Path,nargs='+',required=True);a.add_argument('--datasets',default='analytical36,analytical40-1025x1031');a.add_argument('--python',type=Path,default=Path(sys.executable));a.add_argument('--output',type=Path,required=True);a.add_argument('--smoke',action='store_true')
    a=sub.add_parser('run');a.add_argument('--output',type=Path,required=True);a.add_argument('--freeze-sha256',required=True)
    args=p.parse_args();globals()[args.stage](args)
if __name__=='__main__':main()
