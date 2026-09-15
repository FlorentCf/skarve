#!/usr/bin/env python3
"""Freeze, generate, execute and verify the bounded beta2 comparison programme.

Every task runs in a fresh serial worker, containing its retained queries or
batch calls. The parent retains failed receipts and enforces exact task/output
membership. This is an explicit benchmark command, not package installation.
"""
from __future__ import annotations
import argparse
from collections import Counter
import datetime
import importlib.metadata
import json
import math
import os
from pathlib import Path
import platform
import random
import resource
import secrets
import subprocess
import sys
import time

for variable in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS'):
    os.environ[variable]='1'
os.environ['GDAL_CACHEMAX']='64'
os.environ['PROJ_NETWORK']='OFF'

ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'benchmarks'))
from common import (EE_POLICY, NATIVE_POLICY, FIELDS, array_rasters, differences,
                    digest, normalized_file, read_json, rectangle, upstream, write_json)


def source_hashes():
    tracked=subprocess.check_output(['git','ls-files','-z'],cwd=ROOT).decode().split('\0')
    files=[name for name in tracked if name and (name.startswith(('src/','bindings/','native/','benchmarks/beta2/'))
           or name in {'Cargo.toml','Cargo.lock','build.rs','benchmarks/quick_benchmark.py','examples/range_server.py'})]
    files += [str(p.relative_to(ROOT)) for p in Path(__file__).parent.glob('*.py')]
    files += [str(p.relative_to(ROOT)) for p in (ROOT/'tests').glob('exactextract*.py')]
    return {name:digest(ROOT/name) for name in sorted(set(files))}


def tasks(smoke, isolated):
    result=[];rounds=1 if smoke else 3
    for round in range(rounds):
        for family in ('dates','files'):
            for count in ([8] if smoke else [8,64]):
                for method in ('native-fixed','native-layout','ee-feature-sequential','ee-raster-sequential',
                               'auto-ee','auto-both','upstream-feature-sequential','upstream-raster-sequential'):
                    result.append({'family':family,'method':method,'zones':count,'round':round,'calls':2,'output_mode':'numeric'})
                for method in ('native-layout','ee-raster-sequential'):
                    result.append({'family':family,'method':method,'zones':count,'round':round,'calls':2,'output_mode':'full'})
                if not smoke and count==64:
                    if isolated:
                        for strategy in ('feature-sequential','raster-sequential'):
                            result.append({'family':family,'method':'isolated-'+strategy,'zones':count,'round':round,'calls':2,'output_mode':'five-field canonical'})
        for method in ('native-layout','native-prepared','ee-raster-sequential'):
            result.append({'family':'preparation','method':method,'round':round})
        for method in ('native-layout','ee-raster-sequential','ee-feature-sequential'):
            result.append({'family':'remote','method':method,'round':round})
    # p95 is computed from distinct IDs within this retained-source process,
    # never from just three repeats or a mixture of different cache states.
    for state in ('default','warm','overflow'):
        for method in ('native-layout','ee-feature-sequential','ee-raster-sequential',
                       'auto-both','upstream-feature-sequential','upstream-raster-sequential'):
            result.append({'family':'single','state':state,'method':method,'round':0})
    for i,task in enumerate(result):task['id']=f't{i:03d}'
    return result


def freeze(args):
    if args.output.exists():raise FileExistsError('Preserve every previous programme')
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    from raster_engine_lab import resolve_library
    library=resolve_library(args.library)
    if args.artifacts:
        package=read_json(args.artifacts/'manifest.json')
        assert digest(library)==package['library_sha256']
    else:package=None
    if args.expected_library_sha256:assert digest(library)==args.expected_library_sha256
    source=source_hashes();seed=secrets.randbelow(2**48) if args.seed is None else args.seed
    assert 0<=seed<2**63
    cpu=next((line.split(':',1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
              if line.startswith('model name')),'unavailable')
    versions={name:importlib.metadata.version(name) for name in ('numpy','rasterio','shapely','exactextract')}
    assert versions['exactextract']=='0.3.0'
    manifest={'schema':'skarve_beta2_programme_v1','seed':seed,'smoke':args.smoke,
        'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'harness_source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'source_hashes':source,'library_sha256':digest(library),
        'package_manifest_sha256':digest(args.artifacts/'manifest.json') if args.artifacts else None,
        'package_source_commit':package.get('source_commit') if package else None,
        'binding_mode':'explicit checkout development' if args.checkout_bindings else 'installed package',
        'recorded_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'versions':versions|{'python':platform.python_version()},
        'host':{'system':platform.system(),'kernel':platform.release(),'machine':platform.machine(),
                'cpu_model':cpu,'logical_cpus':os.cpu_count()},
        'limits':{'worker_address_space_bytes':2<<30,'gdal_cache_bytes':64<<20,
                  'serial_timed_workers':1,'threads':1,'timeout_seconds_per_worker':180,
                  'upstream_max_cells':1_000_000,'native_decoded_cache_bytes':[0,1<<20],
                  'native_and_ee_batch_working_bytes':1<<30,
                  'batch_max_contributions_default':1_000_000_000,
                  'largest_batch_contribution_admission_upper_bound':512*512*(4 if args.smoke else 24)*(8 if args.smoke else 64),
                  'contribution_bound_is_actual_cell_visits':False},
        'tasks':tasks(args.smoke,bool(args.isolated_adapter)),
        'scope':'Generated sources only; no external raster/cloud/application access. Freeze precedes source and geometry generation. Same physical host; uncontrolled OS cache.',
        'statistical_definitions':{'batch':'Three independent worker lifecycles per method/shape. Each lifecycle contains first and repeated aligned call. Medians and min/max are observed spread, not confidence intervals. Full native/EE lanes request the identical five fields. Native numeric and native-selected auto require integer count as a sixth field; these are supplemental competent controls, projected to five after the complete response is charged.',
            'single':'100 distinct geometries per retained-source state/method in one process; linear-interpolated empirical p95 within that sequence. Source registration and distinct warmup separately charged.',
            'ratio':'Ratio of the named methods median costs, never an unlabeled median of per-query ratios.',
            'preparation':'Measured cumulative costs across the identical ordered cohort, including build/register. No linear extrapolation labelled as observed break-even.'}}
    write_json(args.output/'freeze.json',manifest)
    print(json.dumps({'frozen':True,'tasks':len(manifest['tasks']),'seed':seed,'library_sha256':manifest['library_sha256']}))


def generate(folder,manifest):
    import quick_benchmark as base
    count=8 if manifest['smoke'] else 64
    fixture_manifest={'seed':manifest['seed'],'date_slices':4 if manifest['smoke'] else 24,
                      'batch_zones':count,'single_queries_per_pattern':12 if manifest['smoke'] else 100}
    fixtures=base.make_fixtures(folder,fixture_manifest)
    # Keep the generated base receipt immutable. Extensions have their own
    # explicit file and are merged only in the worker's input view.
    rng=random.Random(manifest['seed']+9103)
    prep=[]
    for i in range(8 if manifest['smoke'] else 32):
        a,b=rng.uniform(1,65),rng.uniform(1,65)
        prep.append({'id':f'prep{i:03d}','version':'beta2-generated-v1','geometry':rectangle(a,b,a+rng.uniform(350,440),b+rng.uniform(350,440))})
    remote=[]
    for i,(state,a,b) in enumerate([('first',20.,20.),('warm',21.25,21.5),('warm',22.5,22.25),
                                  ('far',276.,20.),('far',20.,276.),('far',276.,276.),('revisit',20.5,20.5)]):
        remote.append({'id':f'remote{i}','state':state,'geometry':rectangle(a,b,a+7.25,b+8.5)})
    fixtures['preparation_zones']=prep;fixtures['remote_zones']=remote
    write_json(folder/'extended-fixtures.json',fixtures)
    return fixtures


def expected(folder,fixtures):
    import quick_benchmark as base
    values,elapsed=base.oracle(folder,fixtures)
    extra=dict(fixtures)
    extra['zones']=fixtures['preparation_zones']+fixtures['remote_zones'];extra['single_zones']=[]
    extra['sources']=[s for s in fixtures['sources'] if s['id'] in ('single','file1')]
    additional,extra_ms=base.oracle(folder,extra);values.update(additional)
    return values,elapsed+extra_ms


def check_record(record, task, fixtures, oracle):
    calls=record.get('calls',[])
    if task['family'] in ('dates','files'):
        wanted_calls=list(range(task['calls']));assert [c.get('call') for c in calls]==wanted_calls
        source_ids=[f'date{i:02d}' for i in range(next(s['bands'] for s in fixtures['sources'] if s['id']=='dates'))] if task['family']=='dates' else [f'file{i}' for i in range(6)]
        keys={(z['id'],source) for z in fixtures['zones'][:task['zones']] for source in source_ids}
    else:
        group=fixtures['preparation_zones'] if task['family']=='preparation' else fixtures['remote_zones'] if task['family']=='remote' else [z for z in fixtures['single_zones'] if z['pattern']==('scatter' if task['state']=='overflow' else 'hot')]
        assert [c.get('query') for c in calls]==[z['id'] for z in group], 'Missing/duplicate/reordered query calls'
        keys=None
    failures=[];checks=0;selected=[]
    for call in calls:
        if not task['method'].startswith(('upstream-','isolated-')):
            p=call.get('provenance')
            assert isinstance(p,dict), 'Missing executed backend provenance'
            assert p['selected_backend'] in ('native','exactextract')
            assert p['numerical_policy']==(NATIVE_POLICY if p['selected_backend']=='native' else EE_POLICY)
            if task['method'].startswith('native') or task['method']=='auto-native':assert p['selected_backend']=='native'
            if task['method'].startswith('ee-') or task['method']=='auto-ee':assert p['selected_backend']=='exactextract'
            selected.append(p['selected_backend'])
        wanted=keys or {(call['query'],'file1' if task['family']=='remote' else 'single')}
        actual_keys=[(r['zone'],r['source']) for r in call['answers']]
        assert len(actual_keys)==len(wanted) and set(actual_keys)==wanted,'Missing/duplicate/unknown useful output rows'
        for row in call['answers']:
            source=row['source'];group='single' if task['family']=='single' else 'batch'
            if source.startswith('date'):target=[oracle[(group,'dates',row['zone'])][int(source[4:])]]
            else:target=oracle[(group,source,row['zone'])]
            if task['family']=='remote':target=target[:1]
            errors=differences(row['bands'],target,strict=True)
            failures.extend({'query':row['zone'],'source':source,**error} for error in errors)
            checks += len(target)*5
    # Strict failures are fatal only for native-selected execution. Delegated
    # output must instead pass its matching upstream comparison below.
    return {'checks':checks,'strict_differences':failures,'shape_complete':True,'selected_backends':sorted(set(selected))}


def paired_upstream(records):
    references={}
    for record in records:
        task=record['task']
        if record['passed_execution'] and task['family'] in ('dates','files') and task['method'].startswith('upstream-'):
            references[(task['family'],task['zones'],task['round'],task['method'].removeprefix('upstream-'))]=record
    rows=[]
    for record in records:
        task=record['task'];method=task['method']
        selected=record.get('validation',{}).get('selected_backends',[])
        if not record['passed_execution'] or task['family'] not in ('dates','files') or not (method.startswith(('ee-','isolated-')) or selected==['exactextract']):continue
        strategy=method.removeprefix('ee-').removeprefix('isolated-') if method.startswith(('ee-','isolated-')) else 'raster-sequential'
        key=(task['family'],task['zones'],task['round'],strategy)
        if key not in references:
            rows.append({'task':task['id'],'passed':False,'reason':'Matching natural upstream task unavailable'});continue
        reference=references[key];errors=[];checks=0
        for actual,control in zip(record['calls'],reference['calls']):
            a={(r['zone'],r['source']):r['bands'] for r in actual['answers']}
            b={(r['zone'],r['source']):r['bands'] for r in control['answers']}
            assert a.keys()==b.keys()
            for key in a:
                errors.extend({'zone':key[0],'source':key[1],**e} for e in differences(a[key],b[key],strict=False));checks+=len(b[key])*5
        rows.append({'task':task['id'],'control':reference['task']['id'],'passed':not errors,'checks':checks,'differences':errors})
    return rows


def independent_single_pairs(folder, fixtures, records):
    """Unscored same-feature upstream controls for retained query families."""
    started=time.perf_counter();cache={};source_cache={};pairs=[]
    groups={'single':fixtures['single_zones'],'preparation':fixtures['preparation_zones'],'remote':fixtures['remote_zones']}
    for record in records:
        task=record['task']
        if not record['passed_execution'] or task['family'] not in groups:continue
        if record.get('validation',{}).get('selected_backends')!=['exactextract']:continue
        strategy=task['method'].removeprefix('ee-') if task['method'].startswith('ee-') else 'raster-sequential'
        source_id='file1' if task['family']=='remote' else 'single'
        if source_id not in source_cache:
            source=next(s for s in fixtures['sources'] if s['id']==source_id)
            normalized=normalized_file(folder/source['path'],bands=[0])
            source_cache[source_id]=(array_rasters(normalized),normalized['crs'])
        rasters,crs=source_cache[source_id];zones={z['id']:z for z in groups[task['family']]}
        errors=[];checks=0
        for call in record['calls']:
            key=(task['family'],source_id,call['query'],strategy)
            if key not in cache:
                cache[key]=upstream(rasters,[zones[call['query']]],crs,strategy)[0]['bands']
            actual=call['answers'][0]['bands']
            errors.extend({'query':call['query'],**e} for e in differences(actual,cache[key],strict=False));checks+=len(actual)*5
        pairs.append({'task':task['id'],'passed':not errors,'checks':checks,'differences':errors})
    return {'scope':'Unscored independent natural upstream calls on identical normalized f64 validity, one feature per call matching installed singles. Invalid cells alone are copied to explicit NaN nodata to avoid the observed pinned cropped masked-array raster-path issue; every valid f64 bit is asserted unchanged. Full decode, snapshot copy and control setup are included in this separate oracle timer.',
            'elapsed_ms':(time.perf_counter()-started)*1000,'distinct_controls':len(cache),'pairs':pairs}


def run(args):
    folder=args.output.resolve();manifest=read_json(folder/'freeze.json')
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    from raster_engine_lab import resolve_library
    assert digest(resolve_library(args.library))==manifest['library_sha256']
    assert source_hashes()==manifest['source_hashes'],'Source changed after freeze'
    assert not (folder/'programme.json').exists(), 'Use a fresh programme'
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    began=time.perf_counter();fixtures=generate(folder,manifest);oracle,oracle_ms=expected(folder,fixtures)
    plan=list(manifest['tasks']);random.Random(manifest['seed']+99).shuffle(plan)
    taskdir=folder/'tasks';taskdir.mkdir();records=[];failures=[]
    for task in plan:
        path=taskdir/(task['id']+'.json');write_json(path,task)
        output=taskdir/(task['id']+'-result.json.gz')
        command=[sys.executable,str(Path(__file__).with_name('worker.py')),'--folder',str(folder),
                 '--task',str(path),'--output',str(output)]
        if args.library:command+=['--library',str(args.library)]
        if args.checkout_bindings:command+=['--checkout-bindings']
        if args.isolated_adapter:command+=['--isolated-adapter',str(args.isolated_adapter)]
        start=time.perf_counter();process=None
        try:
            process=subprocess.run(command,capture_output=True,text=True,timeout=180)
            elapsed=(time.perf_counter()-start)*1000
            if output.exists():record=read_json(output)
            else:record={'task':task,'passed_execution':False,'errors':[{'error':'Worker did not produce a complete receipt'}]}
            assert record['task']==task
            if process.returncode != 0:record['passed_execution']=False
            record['process_wall_ms']=elapsed;record['exit_code']=process.returncode
            if record['passed_execution']:record['validation']=check_record(record,task,fixtures,oracle)
        except Exception as error:
            record={'task':task,'passed_execution':False,'errors':[{'type':type(error).__name__,'error':str(error).replace(str(folder),'<benchmark-output>')}],
                    'process_wall_ms':(time.perf_counter()-start)*1000}
        records.append(record)
        print(json.dumps({'task':task['id'],'family':task['family'],'method':task['method'],'passed_execution':record['passed_execution']}),flush=True)
    assert Counter(r['task']['id'] for r in records)==Counter(t['id'] for t in manifest['tasks'])
    pairs=paired_upstream(records)
    single_pairs=independent_single_pairs(folder,fixtures,records)
    for record in records:
        if not record['passed_execution']:failures.append({'task':record['task']['id'],'reason':'execution/shape failure','details':record['errors']})
        elif record['validation']['selected_backends']==['native'] and record['validation']['strict_differences']:
            failures.append({'task':record['task']['id'],'reason':'native strict mismatch'})
    failures += [{'task':p['task'],'reason':'matching-upstream mismatch or missing control'} for p in pairs if not p['passed']]
    failures += [{'task':p['task'],'reason':'matching single-feature upstream mismatch'} for p in single_pairs['pairs'] if not p['passed']]
    unchanged=source_hashes()==manifest['source_hashes'] and digest(resolve_library(args.library))==manifest['library_sha256']
    unchanged &= all(digest(folder/s['path'])==s['sha256'] for s in fixtures['sources'])
    if not unchanged:failures.append({'reason':'Frozen implementation or generated source changed'})
    result={'schema':'skarve_beta2_programme_results_v1','passed':not failures,'freeze':manifest,
            'freeze_sha256':digest(folder/'freeze.json'),'fixtures_sha256':digest(folder/'extended-fixtures.json'),
            'records':records,'upstream_pairs':pairs,'independent_single_pairs':single_pairs,'failures':failures,'all_frozen_inputs_unchanged':unchanged,
            'oracle_ms':oracle_ms,'orchestration_ms':(time.perf_counter()-began)*1000,
            'limits':manifest['limits'],'limitations':['One physical host, uncontrolled OS cache; no cloud or application comparison.',
                'Natural exactextract uses its separately built Python wheel and source adapter. Same-build C++ diagnostic is a separate lane.',
                'Direct native strict checks and matching-upstream checks are separate; empirical agreement does not enable strict routing.',
                'Old isolated control has a separate1GiB worker and16MiB GDAL cache; costs and guarantees differ from embedded execution.',
                'Worker process wall includes interpreter/import and harness costs; lifecycle/call timers are nested, not additive.']}
    write_json(folder/'programme.json',result)
    print(json.dumps({'passed':result['passed'],'tasks':len(records),'upstream_pairs':len(pairs),'failures':len(failures)}))
    return 0 if result['passed'] else 1


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('stage',choices=['freeze','run']);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--artifacts',type=Path);p.add_argument('--library',type=Path)
    p.add_argument('--expected-library-sha256');p.add_argument('--seed',type=int)
    p.add_argument('--smoke',action='store_true');p.add_argument('--isolated-adapter',type=Path)
    p.add_argument('--checkout-bindings',action='store_true');args=p.parse_args()
    return freeze(args) if args.stage=='freeze' else run(args)


if __name__=='__main__':raise SystemExit(main())
