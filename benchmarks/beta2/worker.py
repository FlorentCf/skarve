#!/usr/bin/env python3
"""One bounded benchmark lane per process; never one process per polygon."""
from __future__ import annotations
import argparse
from collections import Counter
import contextlib
import importlib
import json
import os
from pathlib import Path
import resource
import sys
import time

for variable in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS'):
    os.environ[variable] = '1'
os.environ['GDAL_CACHEMAX'] = '64'
os.environ['PROJ_NETWORK'] = 'OFF'
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'benchmarks'))
sys.path.insert(0,str(ROOT/'examples'))
from common import (CRS, EE_POLICY, FIELDS, NATIVE_POLICY, digest, native_bands,
                    read_json, upstream, write_json)


def now(): return time.perf_counter()
def ms(start): return (now()-start)*1000


def query_metrics(value):
    # Preserve each backend's actual diagnostics without inventing a shared
    # counter schema. Delegated calls use work/source_access, not streaming.
    return {key:value[key] for key in ('streaming','work','source_access','timing_ms','persistent') if key in value}


def sink(rows):
    start = now()
    encoded = json.dumps(rows,sort_keys=True,separators=(',',':'),allow_nan=False).encode()
    return {'sink_bytes':len(encoded),'sink_ms':ms(start)}


def selection(fixtures, family):
    return [s for s in fixtures['sources'] if (s['id']=='dates' if family=='dates'
            else s['id'].startswith('file') if family=='files' else s['id']=='single')]


def options(method):
    if method.startswith('native'):
        return {'backend':'native'}
    if method == 'auto-native':
        return {'backend':'auto','accepted_policies':[NATIVE_POLICY]}
    if method == 'auto-both':
        return {'backend':'auto','accepted_policies':[NATIVE_POLICY,EE_POLICY]}
    if method == 'auto-ee':
        return {'backend':'auto','accepted_policies':[EE_POLICY]}
    assert method.startswith('ee-')
    return {'backend':'exactextract','backend_options':{'strategy':method.removeprefix('ee-'),
            'max_cells_in_memory':1_000_000}}


def requested_statistics(method,mode):
    # Native's established numeric wire schema includes integer count. Keep it
    # as a labelled competent six-field control, and retain full/five lanes.
    return list(FIELDS)+(['count'] if mode=='numeric' and
        (method.startswith('native') or method=='auto-both') else [])


def canonical_flat(rows, sources, zones, family):
    expected = Counter(z['id'] for z in zones)
    assert Counter(row['zone'] for row in rows)==expected
    count = sum(s['bands'] for s in sources)
    output = []
    for row in rows:
        assert len(row['bands'])==count
        band=0
        for source in sources:
            values=row['bands'][band:band+source['bands']]
            band += source['bands']
            if family=='dates':
                output.extend({'zone':row['zone'],'source':f'date{i:02d}','bands':[v]} for i,v in enumerate(values))
            else:output.append({'zone':row['zone'],'source':source['id'],'bands':values})
    return output


def native_batch(folder, fixtures, task, library):
    from skarve import Skarve
    sources=selection(fixtures,task['family']);zones=fixtures['zones'][:task['zones']]
    start=now();engine=Skarve(library);opened=now();slices=[]
    try:
        for source in sources:
            if task['family']=='dates':
                for first in range(0,source['bands'],20):
                    key=f'dates{first}'
                    engine.register_source({'location':str(folder/source['path']),
                        'bands':list(range(first,min(first+20,source['bands'])))},id=key)
                    slices.extend({'id':f'date{band:02d}','source':key,'bands':[band-first]}
                                  for band in range(first,min(first+20,source['bands'])))
            else:
                engine.register_source({'location':str(folder/source['path'])},id=source['id'])
                slices.append({'id':source['id'],'source':source['id']})
        setup=ms(start)
        expected_band_ids={s['id']:(s['bands'] if 'bands' in s else
            list(range(next(source['bands'] for source in sources if source['id']==s['source'])))) for s in slices}
        job={'zones':[{k:z[k] for k in ('id','version','geometry')} for z in zones],
             'slices':slices,'crs':CRS,
             'output_mode':task.get('output_mode','numeric'),
             'budget':{'working_bytes':1<<30},
             'options':{'statistics':requested_statistics(task['method'],task.get('output_mode','numeric'))},**options(task['method'])}
        if task['method'].startswith('native'):
            job.update({'tile_edge':64,'geometry_layout':'auto',
                        'window_policy':'fixed' if task['method']=='native-fixed' else 'source_layout'})
        records=[]
        for repeat in range(task.get('calls',2)):
            query=now();rows=[];page_count=0;provenance=None;metrics=None;complete=False
            for page in engine.cleave(job,max_rows=128,checkpoint_interval=2**30):
                page_count+=1;complete=page['complete'];metrics=page['metrics']
                provenance=page.get('provenance',page.get('descriptor',{}).get('provenance'))
                rows.extend({'zone':row['zone_id'],'source':row['slice_id'],
                             'bands':native_bands(row,expected_band_ids[row['slice_id']])} for row in page['rows'])
            assert complete, 'Incomplete native batch'
            consumed=sink(rows)
            records.append({'call':repeat,'elapsed_ms':ms(query),'answers':rows,
                'metrics':metrics,'provenance':provenance,'pages':page_count,**consumed})
    finally:
        closed=now();engine.close();close_ms=ms(closed)
    return {'engine_create_ms':(opened-start)*1000,'setup_ms':setup,'close_ms':close_ms,
            'lifecycle_ms':ms(start),'calls':records,'source_count':len(sources),
            'job_budget':job['budget'],'logical_slices':len(slices),
            'requested_statistics':job['options']['statistics'],
            'useful_projection_fields':list(FIELDS)}


def natural_batch(folder, fixtures, task):
    import rasterio
    from exactextract.raster import RasterioRasterSource
    sources=selection(fixtures,task['family']);zones=fixtures['zones'][:task['zones']]
    start=now();records=[]
    with contextlib.ExitStack() as stack:
        stack.enter_context(rasterio.Env(GDAL_CACHEMAX=64<<20))
        rasters=[]
        for source in sources:
            dataset=stack.enter_context(rasterio.open(folder/source['path']))
            # Generated timing inputs have identity interpretation. Calibration
            # handles nonidentity scaling with normalized f64 controls.
            assert all(s==1 for s in dataset.scales) and all(o==0 for o in dataset.offsets)
            rasters.extend(RasterioRasterSource(dataset,i+1,name=f'{source["id"]}_{i}') for i in range(source['bands']))
        setup=ms(start)
        for repeat in range(task.get('calls',2)):
            query=now();raw=upstream(rasters,zones,dataset.crs.wkt,task['method'].removeprefix('upstream-'))
            rows=canonical_flat(raw,sources,zones,task['family']);consumed=sink(rows)
            records.append({'call':repeat,'elapsed_ms':ms(query),'answers':rows,**consumed})
        closed=now()
    return {'setup_ms':setup,'close_ms':ms(closed),'lifecycle_ms':ms(start),'calls':records,
            'source_count':len(sources),'interpretation':'Natural Rasterio original files; identity scale/offset and finite dyadic f32 values match f64 normalization exactly.'}


def isolated_batch(folder, fixtures, task, adapter):
    if adapter is None:raise RuntimeError('Private historical isolated adapter path was not supplied')
    adapter=Path(adapter).resolve();sys.path.insert(0,str(adapter.parents[1]))
    module=importlib.import_module('integration.exactextract_backend')
    sources=selection(fixtures,task['family']);zones=fixtures['zones'][:task['zones']]
    start=now();session=module.ExactExtractSession(policy=EE_POLICY);records=[]
    try:
        for source in sources:session.register_file(source['id'],folder/source['path'],bands=list(range(source['bands'])))
        setup=ms(start)
        for repeat in range(task.get('calls',2)):
            query=now();value=session.measure([s['id'] for s in sources],
                [{'id':z['id'],'geometry':z['geometry']} for z in zones],crs=CRS,
                statistics=FIELDS,strategy=task['method'].removeprefix('isolated-'))
            assert list(value.values.shape)==[len(zones),sum(s['bands'] for s in sources),5]
            raw=[{'zone':z['id'],'bands':[{field:float(value.values[i,b,k]) if value.defined[i,b,k] else None
                  for k,field in enumerate(FIELDS)} for b in range(value.values.shape[1])]} for i,z in enumerate(zones)]
            rows=canonical_flat(raw,sources,zones,task['family']);consumed=sink(rows)
            records.append({'call':repeat,'elapsed_ms':ms(query),'answers':rows,'metrics':value.metadata,**consumed})
    finally:
        closed=now();session.close();close_ms=ms(closed)
    return {'startup_ms':session.startup_ms,'setup_ms':setup,'close_ms':close_ms,
            'lifecycle_ms':ms(start),'calls':records,'source_count':len(sources),
            'adapter_hashes':{p.name:digest(p) for p in sorted(adapter.glob('*.py'))},
            'scope':'Private historical development control; not included in the public source package.',
            'limits':{'worker_address_space_bytes':1<<30,'worker_gdal_cache_bytes':16<<20,
                      'parent_address_space_bytes':2<<30,'max_cells_in_memory':262144}}


def singles(folder, fixtures, task, library):
    from skarve import Skarve
    import rasterio
    from exactextract.raster import RasterioRasterSource
    from shapely.affinity import translate
    from shapely.geometry import shape,mapping
    spec=selection(fixtures,'single')[0]
    pattern='scatter' if task['state']=='overflow' else 'hot'
    zones=[z for z in fixtures['single_zones'] if z['pattern']==pattern]
    start=now();rows=[];warmup=None;engine=None
    with contextlib.ExitStack() as stack:
        if task['method'].startswith('upstream-'):
            stack.enter_context(rasterio.Env(GDAL_CACHEMAX=64<<20))
            dataset=stack.enter_context(rasterio.open(folder/spec['path']))
            raster=RasterioRasterSource(dataset,1)
            def measure(geometry):
                value=upstream([raster],[{'id':'one','geometry':geometry}],dataset.crs.wkt,
                    task['method'].removeprefix('upstream-'))[0]['bands']
                return value,{},None,None
        else:
            engine=stack.enter_context(Skarve(library))
            engine.call({'op':'configure_file_cache','bytes':0 if task['state']=='default' else 1<<20})
            source=stack.enter_context(engine.infuse(folder/spec['path'],id='single'))
            def measure(geometry):
                value=source.carve(zone=geometry,metrics=list(FIELDS),**options(task['method']))
                return native_bands(value,[0]),query_metrics(value),value.get('provenance'),engine.last_call_profile
        setup=ms(start)
        if task['state']=='warm':
            geom=mapping(translate(shape(zones[0]['geometry']),xoff=.03125,yoff=.0625))
            query=now();answer,metrics,provenance,profile=measure(geom);sink(answer)
            warmup={'elapsed_ms':ms(query),'geometry':geom,'metrics':metrics}
        for zone in zones:
            query=now();answer,metrics,provenance,profile=measure(zone['geometry'])
            canonical=[{'zone':zone['id'],'source':'single','bands':answer}];consumed=sink(canonical)
            rows.append({'query':zone['id'],'elapsed_ms':ms(query),'answers':canonical,
                         'metrics':metrics,'provenance':provenance,'binding_profile':profile,**consumed})
        closed=now()
    return {'setup_ms':setup,'warmup':warmup,'close_ms':ms(closed),'lifecycle_ms':ms(start),'calls':rows}


def preparation(folder,fixtures,task,library,output):
    from skarve import Skarve
    spec=selection(fixtures,'single')[0];start=now();build={};calls=[]
    with Skarve(library) as engine:
        with engine.infuse(folder/spec['path'],id='s') as source:
            registration=ms(start);index=None
            if task['method']=='native-prepared':
                location=output.parent/(output.stem+'-index');began=now()
                built=source.ward(location,tile_edge=64,boundary_source='original')
                build={'build_ms':ms(began),'index_bytes':sum(p.stat().st_size for p in location.rglob('*') if p.is_file()),
                       'normalized_values_bytes':0,'metrics':built}
                began=now();index=source.open_index(location,id='prepared');build['index_registration_ms']=ms(began)
            try:
                for zone in fixtures['preparation_zones']:
                    began=now()
                    value=(index.measure(zone['geometry'],CRS,statistics=list(FIELDS)) if index else
                           source.carve(zone=zone['geometry'],metrics=list(FIELDS),**options(task['method'])))
                    rows=[{'zone':zone['id'],'source':'single','bands':native_bands(value,[0])}];consumed=sink(rows)
                    calls.append({'query':zone['id'],'elapsed_ms':ms(began),'answers':rows,
                                  'metrics':query_metrics(value),'provenance':value.get('provenance'),**consumed})
            finally:
                if index:index.close()
            closed=now()
    return {'setup_ms':registration,**build,'close_ms':ms(closed),'lifecycle_ms':ms(start),'calls':calls}


def remote(folder,fixtures,task,library):
    from skarve import Skarve
    from range_server import served
    spec=next(s for s in fixtures['sources'] if s['id']=='file1')
    zones=fixtures['remote_zones'];start=now();calls=[]
    with served(folder/spec['path']) as server, Skarve(library) as engine:
        engine.call({'op':'configure_file_cache','bytes':1<<20})
        spec_options={'location':server.url,'bands':[0], 'http':{'allow_http':True,
            'cache_bytes':0,'max_requests':256,'max_range_bytes':1<<20,'max_download_bytes':8<<20}}
        with engine.infuse(spec_options,id='remote') as source:
            setup=ms(start)
            for zone in zones:
                before=len(server.snapshot());began=now()
                value=source.carve(zone=zone['geometry'],metrics=list(FIELDS),**options(task['method']))
                rows=[{'zone':zone['id'],'source':'file1','bands':native_bands(value,[0])}];consumed=sink(rows)
                calls.append({'query':zone['id'],'state':zone['state'],'elapsed_ms':ms(began),'answers':rows,
                    'metrics':query_metrics(value),'provenance':value.get('provenance'),
                    'server':server.snapshot()[before:],**consumed})
            server.etag='"generated-mutation"';rejected=False
            try:source.carve(zone=zones[0]['geometry'],metrics=list(FIELDS),**options(task['method']))
            except Exception:rejected=True
            assert rejected, 'Source mutation accepted'
            closed=now()
    return {'setup_ms':setup,'close_ms':ms(closed),'lifecycle_ms':ms(start),'calls':calls,
            'mutation_rejected':rejected,'server_requests':server.snapshot(),
            'scope':'Same physical host loopback only; no cloud/provider inference. Conditional requests required by the server; natural external remote adapters are not represented.'}


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--folder',type=Path,required=True);p.add_argument('--task',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True);p.add_argument('--library',type=Path)
    p.add_argument('--isolated-adapter',type=Path);p.add_argument('--checkout-bindings',action='store_true')
    args=p.parse_args();assert not args.output.exists()
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    task=read_json(args.task);fixtures=read_json(args.folder/'extended-fixtures.json')
    result={'schema':1,'task':task,'passed_execution':False,'errors':[]}
    began=now()
    try:
        if task['family'] in {'dates','files'}:
            if task['method'].startswith('upstream-'):value=natural_batch(args.folder,fixtures,task)
            elif task['method'].startswith('isolated-'):value=isolated_batch(args.folder,fixtures,task,args.isolated_adapter)
            else:value=native_batch(args.folder,fixtures,task,args.library)
        elif task['family']=='single':value=singles(args.folder,fixtures,task,args.library)
        elif task['family']=='preparation':value=preparation(args.folder,fixtures,task,args.library,args.output)
        elif task['family']=='remote':value=remote(args.folder,fixtures,task,args.library)
        else:raise ValueError('Unsupported benchmark family')
        result.update(value);result['passed_execution']=True
    except Exception as error:
        message=str(error).replace(str(args.folder),'<generated-inputs>').replace(str(ROOT),'<source-checkout>')
        if args.isolated_adapter:message=message.replace(str(args.isolated_adapter),'<private-isolated-control>')
        result['errors'].append({'type':type(error).__name__,'error':message})
    result.update({'worker_after_import_ms':ms(began),'peak_process_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,
                   'address_space_cap_bytes':2<<30,'source_hashes':{s['path']:digest(args.folder/s['path']) for s in fixtures['sources']},
                   'worker_sha256':digest(__file__)})
    write_json(args.output,result)
    print(json.dumps({'task':task['id'],'passed_execution':result['passed_execution'],'calls':len(result.get('calls',[]))}))
    return 0 if result['passed_execution'] else 1


if __name__=='__main__':raise SystemExit(main())
