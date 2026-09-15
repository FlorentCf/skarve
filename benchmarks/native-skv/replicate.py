#!/usr/bin/env python3
"""Portable, separately identified replication of the frozen native programme.

No data downloads, source conversion, route search or retries. Preparation is explicit.
The measured d15 observations remain unchanged. New runtime/object hashes are recorded.
"""
import argparse,copy,fcntl,gzip,hashlib,json,os,resource,subprocess,sys,time
from pathlib import Path
HERE=Path(__file__).resolve().parent
sys.path.insert(0,str(HERE/'harness'))
from protocol import LIMITS,expand_task,validate_answers,validate_execution
from common import differences
from range_server import served

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

def freeze(args):
    original=read(HERE/'FROZEN_PROGRAMME.json');wanted=set(args.datasets.split(','));sources={}
    assert wanted and wanted<=set(original['sources']),'Unknown dataset family'
    for path in args.prepared:
        for source in read(path)['sources']:
            if source['id'] in wanted:
                assert source['id'] not in sources,'Duplicate dataset'
                source=copy.deepcopy(source);directory=Path(source['directory']).resolve()
                assert source['logical']==original['sources'][source['id']]['logical'],'Logical pixels/masks/interpretation differ from declared dataset'
                source['directory']=str(directory);sources[source['id']]=source
    assert set(sources)==wanted,'Missing prepared dataset'
    tasks=[t for t in original['tasks'] if t['dataset'] in wanted]
    # This is a qualification smoke, explicitly separate from the complete programme.
    if args.smoke:
        tasks=[t for t in tasks if t['case']=='A' and t['round']==0 and t['regime']=='local' and t['lane'] in ['native-cog','native-skv-summary']]
    objects={}
    for task in tasks:
        source=sources[task['dataset']];v=source['variants'][task['variant']];directory=Path(source['directory'])
        expected=original['sources'][task['dataset']]['variants'][task['variant']]
        for key in ['block','creation_options','image_structure']:
            if key in expected:assert v.get(key)==expected[key],f'Changed layout {task["dataset"]}/{task["variant"]}/{key}'
        checks=[v]
        if task['mode'] in ['index64','index256']:
            assert v[task['mode']]['tile_edge']==expected[task['mode']]['tile_edge']
            checks+=v[task['mode']]['groups']
        for item in checks:
            path=(directory/item['path']).resolve();assert path.is_file() and path.stat().st_size==item['bytes'] and sha(path)==item['sha256'],'Prepared object changed'
            objects[str(path)]={'sha256':item['sha256'],'bytes':item['bytes']}
        if task['variant']=='skv':
            import struct,zlib
            with (directory/v['path']).open('rb') as stream:header=stream.read(16384)
            size=struct.unpack_from('<I',header,24)[0];metadata=json.loads(zlib.decompress(header[64:64+size]))
            assert metadata['chunk_edge']==128 and metadata['band_group']==len(source['logical']['bands'])
            assert metadata['codec']=='deflate' and metadata.get('predictor')=='byte_delta_v1' and metadata.get('payload_layout','band')=='band' and metadata['summaries'] is True,'Changed SKV recipe'
    probe="import json,hashlib,platform; from pathlib import Path; from raster_engine_lab import resolve_library; import skarve; p=Path(resolve_library()).resolve(); print(json.dumps({'library':str(p),'library_sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'skarve':skarve.__version__,'platform':platform.platform()}))"
    runtime=json.loads(subprocess.check_output([str(args.python),'-c',probe],env=env(),text=True))
    runtime['python']=str(args.python.resolve());runtime['baseline_library_sha256']=original['runtime']['library_sha256']
    runtime['same_library_bytes_as_historical']=runtime['library_sha256']==runtime['baseline_library_sha256']
    frozen={**original,'schema':'skarve_public_native_replication_v1','runtime':runtime,'sources':sources,'tasks':tasks,'objects':objects,
            'counts':{'operations':len(tasks),'smoke_only':args.smoke},'replication_note':'New execution identity; never replaces original d15 observations. Same logical data, frozen geometry/control strategies and limits. Changed preparation/runtime bytes are explicitly pinned.',
            'input_hashes':{str(path.resolve()):sha(path) for path in args.prepared},
            'harness_hashes':{str(path.resolve()):sha(path) for path in sorted((HERE/'harness').glob('*.py'))},'driver_sha256':sha(Path(__file__))}
    args.output.mkdir(parents=True,exist_ok=False);write(args.output/'freeze.json',frozen)
    print(json.dumps({'operations':len(tasks),'freeze_sha256':sha(args.output/'freeze.json'),'same_library_as_d15':runtime['same_library_bytes_as_historical'],'smoke_only':args.smoke}))

def run(args):
    root=args.output;assert sha(root/'freeze.json')==args.freeze_sha256,'Wrong frozen programme';f=read(root/'freeze.json')
    assert f['schema']=='skarve_public_native_replication_v1'
    assert not (root/'attempts').exists(),'No retries or replacement of an attempted programme; use a new explicit replication'
    for path,digest in {**f['harness_hashes'],**f['input_hashes']}.items():assert sha(path)==digest,'Frozen code/receipt changed'
    assert sha(__file__)==f['driver_sha256'] and sha(f['runtime']['library'])==f['runtime']['library_sha256']
    generations={}
    for path,item in f['objects'].items():
        p=Path(path);assert p.stat().st_size==item['bytes'] and sha(p)==item['sha256'];s=p.stat();generations[path]=(s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns)
    results=[]
    with (root/'execution.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        for ordinal,spec in enumerate(f['tasks']):
            assert sum(p.stat().st_size for p in root.rglob('*') if p.is_file())<128<<20,'Evidence cap exceeded; completed prefix retained'
            source=f['sources'][spec['dataset']];variant=source['variants'][spec['variant']];directory=Path(source['directory']);path=(directory/variant['path']).resolve();objects={'source'+path.suffix:path}
            if spec['mode'] in ['index64','index256']:
                for i,index in enumerate(variant[spec['mode']]['groups']):
                    if set(index['source_bands'])&set(spec['bands']):objects[f'summary-{i:02d}.rsi']=(directory/index['path']).resolve()
            for p in objects.values():
                s=p.stat();assert (s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns)==generations[str(p)],'Source changed after freeze'
            write(root/'attempts'/f'{ordinal:05d}.json',{'task':spec['id'],'ordinal':ordinal})
            output=root/'workers'/f'{ordinal:05d}.json.gz';output.parent.mkdir(exist_ok=True)
            def action(server):
                task=expand_task(f,spec,server);task_path=root/'tasks'/f'{ordinal:05d}.json';write(task_path,task)
                before=time.perf_counter();cpu=resource.getrusage(resource.RUSAGE_CHILDREN);record={'task_id':spec['id'],'passed':False}
                try:
                    result=subprocess.run([f['runtime']['python'],str(HERE/'harness/worker.py'),'--task',str(task_path),'--output',str(output)],env=env(),capture_output=True,text=True,timeout=LIMITS['timeout_seconds'])
                    record.update(exit_code=result.returncode,stderr=result.stderr[-8192:])
                    if output.exists():
                        worker=read(output);record['passed']=result.returncode==0 and worker.get('passed') is True
                        if record['passed']:validate_answers(worker,task);validate_execution(worker,task)
                        record.update(worker_file=str(output.relative_to(root)),worker_sha256=sha(output))
                        for k in ['source_start_monotonic_ns','consumed_monotonic_ns']:record[k]=worker.get(k)
                except Exception as error:record.update(passed=False,error={'type':type(error).__name__,'message':str(error)})
                after=resource.getrusage(resource.RUSAGE_CHILDREN);record.update(process_wall_ms=(time.perf_counter()-before)*1000,process_cpu_seconds=after.ru_utime+after.ru_stime-cpu.ru_utime-cpu.ru_stime)
                return record
            if spec['regime']=='http0':
                with served(objects,delay_ms=0,mib_s=0,verified_sha256={k:f['objects'][str(p)]['sha256'] for k,p in objects.items()}) as server:record=action(server)
                record['http']=server.snapshot()
            else:record=action(None)
            write(root/'records'/f'{ordinal:05d}.json.gz',record);results.append(record)
            print(json.dumps({'ordinal':ordinal,'task':spec['id'],'passed':record['passed']}),flush=True)
        lookup={r['task_id']:r for r in results};validation=[]
        for task,record in zip(f['tasks'],results):
            check={'task':task['id'],'passed':record['passed'],'differences':[]}
            if record['passed'] and task['backend']=='native':
                refs=[t for t in f['tasks'] if all(t[k]==task[k] for k in ['dataset','case','round','regime']) and t['lane']=='native-cog'];assert len(refs)==1
                ref=lookup[refs[0]['id']]
                if not ref['passed']:check.update(passed=False,reason='Native COG control unavailable')
                else:
                    a=read(root/record['worker_file']);b=read(root/ref['worker_file'])
                    for x,y in zip(a['answers'],b['answers']):check['differences'].extend(differences(x['bands'],y['bands'],strict=True))
                    check['passed']=not check['differences']
            validation.append(check)
        write(root/'complete.json',{'schema':'skarve_public_replication_results_v1','freeze_sha256':args.freeze_sha256,'attempted':len(results),'passed':sum(r['passed'] for r in validation),'all_failures_in_denominator':True,'validation':validation,'new_runtime':f['runtime']})

def main():
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='stage',required=True)
    a=sub.add_parser('freeze');a.add_argument('--prepared',type=Path,nargs='+',required=True);a.add_argument('--datasets',default='analytical36,analytical40-1025x1031');a.add_argument('--python',type=Path,default=Path(sys.executable));a.add_argument('--output',type=Path,required=True);a.add_argument('--smoke',action='store_true')
    a=sub.add_parser('run');a.add_argument('--output',type=Path,required=True);a.add_argument('--freeze-sha256',required=True)
    args=p.parse_args();globals()[args.stage](args)
if __name__=='__main__':main()
