#!/usr/bin/env python3
"""Small installed-package failure/cancellation cost probe, not a throughput benchmark.

Run with --artifacts, --wheelhouse and a fresh --output-dir outside the checkout.
Each cancellation is armed only after source registration. A loopback server
holds one real range request until the consumer cancels, waits 25 ms, and releases
it. This measures cooperative drain including controlled I/O; it cannot establish
CPU-only cancellation latency, hard process termination, RSS limits or p95.
"""
import argparse
import asyncio
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import resource
import shutil
import subprocess
import sys
import threading
import time
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[2]
POLICIES = {'native':'native_grid_planar_fractional','exactextract':'exactextract_fractional_v030'}


def digest(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()


class GateServer(ThreadingHTTPServer):
    daemon_threads = True
    def __init__(self,path):
        self.path=path;self.length=path.stat().st_size;self.etag='"'+digest(path)+'"'
        assert self.length<=16<<20
        self.lock=threading.Lock();self.release=threading.Event();self.release.set()
        self.armed=False;self.entered=False;self.gets=0;self.bytes=0;self.events=[]
        super().__init__(('127.0.0.1',0),GateHandler)


class GateHandler(BaseHTTPRequestHandler):
    def log_message(self,*_):pass
    def reply(self,status,body=b'',extra=()):
        self.send_response(status);self.send_header('Content-Length',str(len(body)))
        for key,value in extra:self.send_header(key,value)
        self.end_headers()
        try:self.wfile.write(body)
        except (BrokenPipeError,ConnectionResetError):pass
    def do_HEAD(self):
        self.send_response(200);self.send_header('Content-Length',str(self.server.length))
        self.send_header('ETag',self.server.etag);self.send_header('Accept-Ranges','bytes');self.end_headers()
    def do_GET(self):
        s=self.server;self.connection.settimeout(5)
        if self.path.startswith('/control/'):
            action=self.path.removeprefix('/control/')
            with s.lock:
                if action=='arm':
                    assert not s.armed;s.armed=True;s.entered=False;s.release.clear()
                elif action=='release':s.release.set()
                elif action!='state':self.reply(404);return
                result={'entered':s.entered,'gets':s.gets,'bytes':s.bytes}
            self.reply(200,json.dumps(result).encode());return
        if self.path!='/source.tif':self.reply(404);return
        match=re.fullmatch(r'bytes=(\d+)-(\d+)',self.headers.get('Range',''))
        if not match or self.headers.get('If-Match')!=s.etag:self.reply(412);return
        start,end=map(int,match.groups());end=min(end,s.length-1)
        if end<start or end-start+1>1<<20:self.reply(416);return
        with s.lock:
            s.gets+=1;s.bytes+=end-start+1
            if s.gets>2048 or s.bytes>64<<20:self.reply(429);return
            held=s.armed
            if held:s.armed=False;s.entered=True;s.events.append({'event':'read_entered','monotonic_ns':time.monotonic_ns(),'start':start,'end':end})
        if held:
            released=s.release.wait(4)
            with s.lock:s.events.append({'event':'read_released','monotonic_ns':time.monotonic_ns(),'explicit_release':released})
            if not released:self.reply(504);return
        with s.path.open('rb') as stream:stream.seek(start);body=stream.read(end-start+1)
        self.reply(206,body,[('ETag',s.etag),('Accept-Ranges','bytes'),('Content-Range',f'bytes {start}-{end}/{s.length}')])


def control(url,action):
    with urlopen(url+'/control/'+action,timeout=5) as response:return json.load(response)


async def python_worker(args):
    from skarve import Skarve,EngineError
    fixture=json.loads((args.folder/'fixture.json').read_text());zone=fixture['remote']['geometries']['hot-a']
    rows=[]
    for backend in args.backends:
        for repeat in range(args.repeats):
            with Skarve() as sk:
                spec={'location':args.url+'/source.tif','bands':[0],'http':{'allow_http':True,
                    'cache_bytes':0,'max_requests':128,'max_range_bytes':1<<20,'max_download_bytes':8<<20}}
                with sk.infuse(spec) as source:
                    opposite=POLICIES['exactextract' if backend=='native' else 'native']
                    before=control(args.url,'state')['gets'];start=time.perf_counter_ns()
                    try:source.carve(zone=zone,metrics=['sum'],backend=backend,numerical_policy=opposite)
                    except EngineError as error:failure=str(error)
                    else:raise AssertionError('Incompatible policy accepted')
                    rejected=time.perf_counter_ns();assert control(args.url,'state')['gets']==before
                    assert 'polic' in failure.lower()
                    control(args.url,'arm');start_query=time.perf_counter_ns()
                    pending=asyncio.create_task(source.carve_async(zone=zone,metrics=['sum'],backend=backend))
                    deadline=time.monotonic()+4
                    while not control(args.url,'state')['entered']:
                        assert not pending.done(),'Query completed before active range-read proof'
                        assert time.monotonic()<deadline,'No active source read'
                        await asyncio.sleep(.001)
                    assert not pending.done()
                    cancelled=time.perf_counter_ns();pending.cancel();initiated=time.perf_counter_ns()
                    await asyncio.sleep(.025);still_active=not pending.done()
                    release_at=time.perf_counter_ns();control(args.url,'release')
                    try:await asyncio.wait_for(pending,timeout=5)
                    except asyncio.CancelledError:outcome='CancelledError'
                    else:raise AssertionError('Cancelled request returned success')
                    drained=time.perf_counter_ns();assert still_active,'Returned before held source read drained'
                    reuse_start=time.perf_counter_ns();same_source_error=None
                    try:result=source.carve(zone=zone,metrics=['sum'],backend=backend)
                    except EngineError as error:same_source_error=str(error)
                    same_source_end=time.perf_counter_ns()
                    # A cancelled remote transport may remain failed. Record it
                    # before explicitly testing session reuse with a fresh reader.
                    source.close()
                    with sk.infuse(spec) as reopened:
                        result=reopened.carve(zone=zone,metrics=['sum'],backend=backend)
                    reuse_end=time.perf_counter_ns()
                    assert result['provenance']['selected_backend']==backend
                    expected=fixture['remote']['expected']['hot-a'][0]['fractional_sum']
                    assert abs(result['bands'][0]['fractional_sum']-expected)<1e-8
                    rows.append({'runtime':'python','backend':backend,'repeat':repeat,
                        'rejection_ms':(rejected-start)/1e6,'rejection_error':failure,'rejection_source_gets':0,
                        'initiate_cancel_ms':(initiated-cancelled)/1e6,'cancel_to_drain_ms':(drained-cancelled)/1e6,
                        'release_to_drain_ms':(drained-release_at)/1e6,'query_total_until_drain_ms':(drained-start_query)/1e6,
                        'controlled_hold_after_cancel_ms':(release_at-cancelled)/1e6,
                        'active_read_observed':True,'still_active_before_release':still_active,
                        'cancel_outcome':outcome,'same_source_reuse_ms':(same_source_end-reuse_start)/1e6,
                        'same_source_reuse_error':same_source_error,
                        'session_reuse_with_new_source_ms':(reuse_end-same_source_end)/1e6,
                        'session_reuse_answer_correct':True})
    print(json.dumps({'schema':1,'runtime':'python','rows':rows},allow_nan=False))


def run(args):
    output=args.output_dir.resolve();output.mkdir(parents=True,exist_ok=False)
    assert not output.is_relative_to(ROOT),'Use a fresh consumer outside the checkout'
    artifacts=args.artifacts.resolve();manifest=json.loads((artifacts/'manifest.json').read_text())
    env={key:os.environ[key] for key in ('PATH','LANG','TZ') if key in os.environ}
    env.update(HOME=str(output/'home'),TMPDIR=str(output/'tmp'),PYTHONNOUSERSITE='1',
        OPENBLAS_NUM_THREADS='1',OMP_NUM_THREADS='1',GDAL_NUM_THREADS='1',GDAL_CACHEMAX='16',PROJ_NETWORK='OFF',
        PIP_DISABLE_PIP_VERSION_CHECK='1',npm_config_update_notifier='false')
    for name in ('home','tmp'):(output/name).mkdir()
    files={name:ROOT/path for name,path in [('failure_cancellation.py','benchmarks/beta2/failure_cancellation.py'),
        ('failure_cancellation.mjs','benchmarks/beta2/failure_cancellation.mjs'),('generate_fixtures.py','examples/generate_fixtures.py')]}
    for name,path in files.items():shutil.copyfile(path,output/name)
    receipt={'schema':1,'passed':False,'artifact_manifest_sha256':digest(artifacts/'manifest.json'),
        'artifact_source_commit':manifest['source_commit'],'library_sha256':manifest['library_sha256'],
        'scope':'Fresh installed Python and Node on the same host; controlled loopback source I/O. Not a namespace sandbox or independent hardware run.',
        'limitations':['Three observations per backend/runtime, no p95 or general latency claim.',
            'Cancellation waits for a real source read deliberately held for at least 25 ms after initiation.',
            'No CPU-only cancellation latency, hard kill or hard RSS guarantee is measured.'],
        'repeats':args.repeats,'script_sha256':{name:digest(path) for name,path in files.items()},'steps':[],'rows':[]}
    def execute(name,command,parse=False):
        def bounds():resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
        value=subprocess.run(list(map(str,command)),cwd=output,env=env,capture_output=True,text=True,timeout=120,preexec_fn=bounds)
        (output/(name+'.stdout')).write_text(value.stdout);(output/(name+'.stderr')).write_text(value.stderr)
        receipt['steps'].append({'name':name,'exit_code':value.returncode,'stdout_sha256':hashlib.sha256(value.stdout.encode()).hexdigest(),
            'stderr_sha256':hashlib.sha256(value.stderr.encode()).hexdigest()})
        if value.returncode:raise RuntimeError(name+' failed; see retained consumer logs')
        return json.loads(value.stdout) if parse else None
    server=None
    try:
        wheel=next(artifacts.glob('*.whl'));npm=next(artifacts.glob('*.tgz'))
        for path in (wheel,npm):
            entry=next(x for x in manifest['artifacts'] if x['name']==path.name)
            assert digest(path)==entry['sha256'] and path.stat().st_size==entry['bytes']
        execute('venv',[sys.executable,'-m','venv',output/'venv']);python=output/'venv/bin/python'
        execute('wheel',[python,'-m','pip','install','--no-index','--no-deps',wheel])
        execute('fixture-dependencies',[python,'-m','pip','install','--no-index','--find-links',args.wheelhouse,'numpy==2.5.3','rasterio==1.5.1'])
        execute('npm',['npm','install','--offline','--ignore-scripts','--omit=optional','--no-audit','--no-fund',npm])
        execute('generate',[python,output/'generate_fixtures.py',output/'data'])
        server=GateServer(output/'data/range.cog.tif');thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        url=f'http://127.0.0.1:{server.server_port}';backends=['native']+(['exactextract'] if manifest['backends']['exactextract'] else [])
        receipt['source_sha256']=digest(server.path)
        value=execute('python-probe',[python,output/'failure_cancellation.py','worker','--url',url,'--folder',output/'data',
            '--repeats',args.repeats,'--backends',*backends],True);receipt['rows'].extend(value['rows'])
        value=execute('node-probe',['node',output/'failure_cancellation.mjs',url,output/'data',str(args.repeats),*backends],True)
        receipt['rows'].extend(value['rows'])
        receipt['passed']=True
    finally:
        if server:
            server.release.set();server.shutdown();server.server_close();receipt['server_events']=server.events
        assert digest(artifacts/'manifest.json')==receipt['artifact_manifest_sha256']
        (output/'receipt.json').write_text(json.dumps(receipt,indent=2,allow_nan=False)+'\n')
    print(json.dumps({'passed':receipt['passed'],'rows':len(receipt['rows']),'receipt':str(output/'receipt.json')}))


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='mode',required=True)
    runner=sub.add_parser('run');runner.add_argument('--artifacts',type=Path,required=True)
    runner.add_argument('--wheelhouse',type=Path,required=True);runner.add_argument('--output-dir',type=Path,required=True)
    worker=sub.add_parser('worker');worker.add_argument('--url',required=True);worker.add_argument('--folder',type=Path,required=True)
    worker.add_argument('--backends',nargs='+',choices=list(POLICIES),required=True)
    for command in (runner,worker):command.add_argument('--repeats',type=int,default=3,choices=range(1,6))
    args=p.parse_args()
    if args.mode=='run':run(args)
    else:asyncio.run(python_worker(args))
