"""Fixed main release protocol. Pure task and receipt checks; no engine imports."""
from copy import deepcopy
from collections import Counter
import hashlib
import json
import math
import random
from pathlib import Path

SEED = 10738291
SOURCE_COMMIT = 'd15b0bccdf87654e6e6bd221ce114fb4f129e42d'
DATASETS = {'analytical36':36, 'analytical40-1025x1031':40, 'real-age36':36, 'worldpop1':1}
LANES = ('native-tiff', 'native-cog', 'native-rsi64', 'native-rsi256', 'native-skv-raw', 'native-skv-summary', 'natural-ee')
FIELDS = ('sum','support','mean','min','max')
LIMITS = {'process_address_space_bytes':2<<30, 'working_bytes':1<<30,
 'decoder_cache_bytes':64<<20, 'gdal_cache_bytes':64<<20, 'workers':1,
 'source_http':{'max_requests':8192,'max_download_bytes':768<<20,'max_range_bytes':4<<20,'cache_bytes':4<<20},
 'index_http':{'max_requests':4096,'max_download_bytes':128<<20,'cache_bytes':0},
 'job_http':{'max_requests':8192,'max_download_bytes':1<<30},
 'native_single_read_bytes':64<<20,'native_batch_tile_bytes':256<<20,'native_index_read_and_tile_bytes':128<<20,
 'native_cumulative_decoded_bytes':2<<30,'native_max_contributions':1_000_000_000,
 'natural_max_cells_in_memory':262144,'timeout_seconds':180}


def require(ok, message='Protocol invariant failed'):
    if not ok:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value,sort_keys=True,separators=(',',':'),allow_nan=False).encode()


def object_sha(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def pin_union(target, additions):
    for path, sha in additions.items():
        require(Path(path).is_absolute() and isinstance(sha,str) and len(sha)==64 and all(c in '0123456789abcdef' for c in sha), 'Malformed pin')
        require(path not in target or target[path]==sha, 'Conflicting pin: '+path)
    target.update(additions)


def natural_contract(dataset, case):
    if dataset=='real-age36' and case=='C': return 'cog128','feature-sequential'
    if dataset=='real-age36' and case=='D': return 'cog256','raster-sequential'
    return 'cogband128','feature-sequential' if case in 'AC' else 'raster-sequential'


def control_map(retained):
    """Select from completed R12 observations only, never new cohort outcomes."""
    require(retained['completed_before_fresh_generation'] is True)
    result={}
    for dataset in DATASETS:
        for case in ('AB' if dataset=='worldpop1' else 'ABCD'):
            for regime in ('local','http0'):
                key='|'.join((dataset,case,regime))
                if dataset=='worldpop1':
                    result[key]={'tiff':'ordinary','cog':'cogband128','rsi64':'cogband128','rsi256':'cogband128','basis':'Original final R11 WorldPop qualified controls'}
                    continue
                cells=[c for c in retained['cells'] if (c['dataset'],c['case'],c['regime'])==(dataset,case,regime)]
                require(len(cells)==1,'Missing/duplicate retained control cell')
                cell=cells[0];obs=cell['native_direct_basis']['observations']
                require(len(obs)==5 and {x['variant'] for x in obs}=={'ordinary','band256','cogband128','cog128','cog256'},'Five original control attempts required')
                choices=[]
                for variants in ({'ordinary','band256'},{'cogband128','cog128','cog256'}):
                    good=[r for r in obs if r['variant'] in variants and r['passed'] is True and r['same_policy_validated'] is True]
                    require(good,'No qualified control in class')
                    require(all(type(r['source_to_consumed_ms']) in (float,int) and math.isfinite(r['source_to_consumed_ms']) and r['source_to_consumed_ms']>0 for r in good))
                    choices.append(min(good,key=lambda r:(r['source_to_consumed_ms'],r['variant'])))
                result[key]={'tiff':choices[0]['variant'],'cog':choices[1]['variant'],'rsi64':cell['native_rsi64'],'rsi256':cell['native_rsi256'],
                             'basis':'R12 retained successful strict-native observations, separate TIFF/true-COG minima',
                             'selected_tasks':[r['task'] for r in choices], 'prior_failures':sum(r['passed'] is not True for r in obs)}
    require(len(result)==28)
    return result


def query_grid(source):
    grid=deepcopy(source['logical']); window=None
    if source['id']=='worldpop1' and grid['width']>2048:
        transform=list(grid['transform']);x=int((-1.52-transform[0])/transform[1])-512;y=int((12.37-transform[3])/transform[5])-512
        window=[x,y,1024,1024];transform[0]+=x*transform[1];transform[3]+=y*transform[5]
        grid.update(width=1024,height=1024,transform=transform)
    return grid,window


def task_factory(sources, controls, workload):
    tasks=[]; geometries={}
    for dataset,bands_count in DATASETS.items():
        source=sources[dataset];grid,window=query_grid(source)
        require(len(source['logical']['bands'])==bands_count and source['id']==dataset)
        for case in ('AB' if bands_count==1 else 'ABCD'):
            for repeat in range(3):
                zones=workload(grid,case,seed=SEED+repeat*997,single_family='interior',many=8)
                require(len(zones)==(1 if case in 'AC' else 8))
                require(all(type(z['version']) is str for z in zones))
                geometry_id='|'.join((dataset,case,str(repeat)))
                geometries[geometry_id]={'zones':zones,'crs':source['crs'],'query_window_xywh':window}
                for regime in ('local','http0'):
                    c=controls['|'.join((dataset,case,regime))]; nv,strategy=natural_contract(dataset,case)
                    descriptors=((c['tiff'],'native',None),(c['cog'],'native',None),(c['rsi64'],'native','index64'),(c['rsi256'],'native','index256'),('skv','native','raw'),('skv','native','summary'),(nv,'upstream',strategy))
                    for lane,(variant,backend,mode) in zip(LANES,descriptors):
                        require(variant in source['variants'],'Required retained object absent')
                        if mode in ('index64','index256'): require(mode in source['variants'][variant], 'Required source-bound index absent')
                        tasks.append({'id':f'{dataset}-{case}-{repeat}-{regime}-{lane}','dataset':dataset,'case':case,'round':repeat,'regime':regime,'lane':lane,
                            'variant':variant,'backend':backend,'mode':mode,'bands':[min(23,bands_count-1)] if case in 'AB' else list(range(bands_count)),
                            'geometry_id':geometry_id,'native_reference':'native-cog','summary_enabled':mode=='summary' if variant=='skv' else None})
    random.Random(SEED+31).shuffle(tasks)
    require(len(tasks)==588 and len({t['id'] for t in tasks})==588 and len(geometries)==42)
    return tasks,geometries


def validate_tasks(frozen):
    require(frozen['seed']==SEED and frozen['limits']==LIMITS and len(frozen['tasks'])==588)
    def from_frozen(grid,case,seed,single_family,many):
        # Factory calls follow canonical dataset/case/repeat order; avoid deriving new geometry on run.
        key=expected_order.pop(0)
        return deepcopy(frozen['geometries'][key]['zones'])
    expected_order=[f'{d}|{c}|{r}' for d,n in DATASETS.items() for c in ('AB' if n==1 else 'ABCD') for r in range(3)]
    tasks,geometries=task_factory(frozen['sources'],frozen['controls'],from_frozen)
    require(tasks==frozen['tasks'] and geometries==frozen['geometries'],'Frozen task/order/geometry descriptor changed')
    require(not expected_order)


def expand_task(frozen,spec,server=None):
    source=frozen['sources'][spec['dataset']];variant=source['variants'][spec['variant']];directory=Path(source['directory']);path=directory/variant['path']
    source_spec={'location':server.url('source'+path.suffix) if server else str(path), 'identity':{'sha256':variant['sha256'],'byte_length':variant['bytes'],'policy':'trusted_manifest'}}
    if server:
        source_spec['http']={'allow_http':True,**LIMITS['source_http'],'boundary_read_ahead':True}
        source_spec['identity']['etag']='"'+variant['sha256']+'"'
    if spec['variant']=='skv': source_spec['use_summaries']=spec['summary_enabled']
    task={**deepcopy(spec),**deepcopy(frozen['geometries'][spec['geometry_id']]),'source':source_spec,'decoded_cache_bytes':64<<20}
    task['expected_installed_runtime']={'path':frozen['runtime']['library'],'sha256':frozen['runtime']['library_sha256']}
    if spec['backend']=='upstream': task['strategy']=spec['mode']
    if spec['mode'] in ('index64','index256'):
        groups=[]
        for i,index in enumerate(variant[spec['mode']]['groups']):
            if set(index['source_bands']).intersection(spec['bands']):
                groups.append({'source_bands':index['source_bands'],'location':server.url(f'summary-{i:02d}.rsi') if server else str(directory/index['path']),'build_id':index['build_id']})
        require(groups)
        require(sorted(b for g in groups for b in g['source_bands'] if b in spec['bands'])==sorted(spec['bands']), 'Incomplete/overlapping source-index band mappings')
        task['index']={'groups':groups}
    return task


def validate_answers(record,task):
    require(record.get('task_id')==task['id'],'Wrong worker task identity')
    rows=record['answers'];wanted=[z['id'] for z in task['zones']]
    require([r['zone'] for r in rows]==wanted,'Missing/duplicate/reordered zone')
    for row in rows:
        require(len(row['bands'])==len(task['bands']))
        for band in row['bands']:
            require(set(band)==set(FIELDS))
            for value in band.values():
                require(value is None or type(value) in (int,float) and math.isfinite(value),'Nonfinite or boolean answer')
    require(record['sink']['sha256']==object_sha(rows) and record['sink']['bytes']==len(canonical(rows)), 'Incomplete output consumption')


def validate_execution(record,task):
    """Proof of actual lane and well-formed complete timings, outside measured bodies."""
    for key in ('source_to_consumed_ms','source_open_ms','query_ms','close_ms','engine_lifecycle_ms','worker_ms','cpu_seconds'):
        value=record[key]
        require(type(value) in (int,float) and math.isfinite(value) and value>=0, 'Invalid recorded cost: '+key)
    require(record['source_to_consumed_ms']>0 and record['source_to_consumed_ms']<=record['worker_ms'], 'Invalid useful/worker interval')
    begin,end=record['source_start_monotonic_ns'],record['consumed_monotonic_ns']
    require(type(begin) is int and type(end) is int and 0<begin<end,'Invalid primary timestamps')
    require(type(record['peak_rss_bytes']) is int and record['peak_rss_bytes']>0 and record['address_space_limit_bytes']==2<<30)
    if task['backend']=='upstream':
        require(record['policy']=='exactextract_fractional_v030','Wrong natural policy')
        return
    validate_native_execution(record,task)


def validate_native_execution(record,task):
    """Native result fields alone, also replayable from prior ordinary API envelopes."""
    provenance=record['provenance']
    require(provenance['selected_backend']=='native' and provenance['requested_backend']=='native', 'Native backend fallback or wrong request')
    require(provenance['numerical_policy']=='native_grid_planar_fractional' and provenance['source_interpretation']=='skarve_normalized_f64_v1','Wrong native numerical/interpretation policy')
    if task['variant']=='skv':
        require(task['source']['use_summaries'] is (task['mode']=='summary'),'Wrong explicit summary request')
        if len(task['zones'])==1:
            access=record['metrics']['source_access'];require(access['format']=='skv' and access['invalidated'] is False)
            counts=[access['metrics']['summary_states_read']]
        else:
            counts=[record['metrics'][k] for k in ('source_summary_records_read','summary_records_read')]
        require(all(type(n) is int and n>=0 for n in counts),'Missing/invalid actual summary counters')
        if task['mode']=='raw':require(all(n==0 for n in counts),'Raw ablation actually read summaries')
        elif task['mode']=='summary':require(any(n>0 for n in counts),'Summary route did not read summaries for the admitted interior-containing workload')
        else:require(False,'Unknown SKV mode')


def prefix_count(records, tasks):
    require(len(records)<=len(tasks))
    for ordinal,record in enumerate(records):
        require(record['ordinal']==ordinal and record['task_id']==tasks[ordinal]['id'] and record['task_sha256']==object_sha(tasks[ordinal]), 'Noncontiguous/mutated completed prefix')
    return len(records)
