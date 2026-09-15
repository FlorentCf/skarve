#!/usr/bin/env python3
"""Credential-free synthetic source/batch confirmation; see benchmarks/README.md."""
from __future__ import annotations
import argparse
from collections import defaultdict
import contextlib
import csv
import datetime
import gzip
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import platform
import random
import resource
import secrets
import statistics
import time

for name in ['OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS']:
 os.environ[name]='1'
os.environ['GDAL_CACHEMAX']='64'
FIELDS=['sum','support','mean','min','max']
NATIVE=['fractional_sum','covered_cell_equivalents','coverage_weighted_mean','min','max']
UPSTREAM=['sum','count','mean','min','max']

def digest(path):
 with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()
def dump(path,value):
 path=Path(path);path.parent.mkdir(parents=True,exist_ok=True)
 if path.exists():raise FileExistsError('Use a fresh output directory: '+str(path))
 with (gzip.open(path,'wt') if str(path).endswith('.gz') else path.open('w')) as stream:json.dump(value,stream,indent=2,allow_nan=False)
def load(path):
 with (gzip.open(path,'rt') if str(path).endswith('.gz') else open(path)) as stream:return json.load(stream)
def core_path(library):
 # Inspection only; computation below uses the documented Engine interface.
 from raster_engine_lab import resolve_library
 return resolve_library(library)
def version(name):
 try:return importlib.metadata.version(name)
 except importlib.metadata.PackageNotFoundError:return None
def freeze(args):
 out=Path(args.output);assert not out.exists()
 library=core_path(args.library)
 if args.expected_library_sha256:assert digest(library)==args.expected_library_sha256
 seed=secrets.randbelow(2**48) if args.seed is None else args.seed
 assert 0<=seed<2**63
 cpu=next((s.split(':',1)[1].strip() for s in Path('/proc/cpuinfo').read_text().splitlines() if s.startswith('model name')),'unavailable')
 manifest={'schema':1,'seed':seed,'recorded_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'library_sha256':digest(library),'harness_sha256':digest(__file__),'quick':args.quick,'versions':{key:version(key) for key in ['skarve-engine','numpy','rasterio','shapely','exactextract']},'host':{'system':platform.system(),'release':platform.release(),'machine':platform.machine(),'cpu_model':cpu,'logical_cpus':os.cpu_count()},'limits':{'process_address_space_bytes':2<<30,'gdal_cache_bytes':64<<20,'single_decoded_cache_bytes':1<<20,'serial_timed_workers':1,'threads':1},'scope':'New deterministic synthetic fixtures and polygons generated only after this installed core was selected. Native-grid planar statistics; direct source-owning calls. No private data, credentials or network required.','source_files':8,'source_grid':[512,512],'batch_zones':8 if args.quick else 64,'date_slices':4 if args.quick else 24,'distinct_files':6,'bands_per_distinct_file':2,'single_queries_per_pattern':12 if args.quick else 100,'rounds':1 if args.quick else 3}
 import ctypes,rasterio,shapely
 native=ctypes.CDLL(str(library));gdal_version=native.GDALVersionInfo;gdal_version.argtypes=[ctypes.c_char_p];gdal_version.restype=ctypes.c_char_p
 manifest['versions'].update({'python':platform.python_version(),'native_gdal':gdal_version(b'RELEASE_NAME').decode(),'rasterio_gdal':rasterio.__gdal_version__,'geos':shapely.geos_version_string})
 manifest['external_empty_normalization']='NaN mean/min/max become null only when fractional support is zero; nonempty nonfinite values fail.'
 dump(out/'freeze.json',manifest)
 print(json.dumps({'freeze':'freeze.json','library_sha256':manifest['library_sha256'],'seed':seed,'quick':args.quick}))

def geometry(seed,count,width=512,height=512):
 from shapely.geometry import box,Polygon,MultiPolygon,mapping
 rng=random.Random(seed);out=[]
 for i in range(count):
  kind=['compact','hole','multipart','detailed','thin','aligned','partial','outside'][i%8]
  x=rng.uniform(12,width-115);y=rng.uniform(12,height-115);w=rng.uniform(8,95);h=rng.uniform(8,95)
  if kind=='thin':w=2**-18;h=120.
  if kind=='aligned':x,y,w,h=[round(v) for v in [x,y,w,h]]
  p=box(x,y,x+w,y+h)
  if kind=='hole':p=p.difference(box(x+w*.2,y+h*.2,x+w*.65,y+h*.65))
  elif kind=='multipart':p=MultiPolygon([box(x,y,x+w*.35,y+h*.35),box(x+w*.65,y+h*.65,x+w,y+h)])
  elif kind=='detailed':p=Polygon([(x+w*.5+w*.5*(1+.05*math.sin(j*7))*math.cos(j*math.tau/32),y+h*.5+h*.5*(1+.05*math.sin(j*7))*math.sin(j*math.tau/32)) for j in range(32)])
  elif kind=='partial':p=box(-w/2,y,w/2,y+h)
  elif kind=='outside':p=box(width+3+i,height+3,width+4+i,height+4)
  assert p.is_valid
  out.append({'id':f'z{i:03d}','version':'synthetic-v1','stratum':kind,'geometry':mapping(p)})
 return out

def make_fixtures(out,manifest):
 import numpy as np
 import rasterio
 from rasterio.transform import from_origin
 fixture=out/'fixtures';fixture.mkdir()
 yy,xx=np.indices((512,512));sources=[];started=time.perf_counter()
 specs=[('single',1,False,'band',1),('dates',manifest['date_slices'],True,'band',128)]
 specs += [(f'file{i}',2,i%3!=2,'pixel' if i%2 else 'band',[64,128,16,256,128,1][i]) for i in range(6)]
 for index,(name,count,tiled,interleave,block) in enumerate(specs):
  path=fixture/(name+'.tif');kwargs={'driver':'GTiff','width':512,'height':512,'count':count,'dtype':'float32','crs':'EPSG:3857','transform':from_origin(0,512,1,1),'nodata':-9999.,'compress':'deflate','interleave':interleave,'tiled':tiled,'blockysize':block}
  if tiled:kwargs['blockxsize']=block
  with rasterio.open(path,'w',**kwargs) as ds:
   for band in range(count):
    values=(((xx*17+yy*31+band*19+index*7+manifest['seed']%307)%307)/16.-7).astype('float32')
    values[(xx+yy*3+band*7+index)%79==0]=-9999.
    ds.write(values,band+1)
  with rasterio.open(path) as ds:layout={'blocks':[list(x) for x in ds.block_shapes],'interleave':ds.profile.get('interleave'),'dtype':ds.dtypes[0],'compression':str(ds.compression)}
  sources.append({'id':name,'path':f'fixtures/{name}.tif','bands':count,'bytes':path.stat().st_size,'sha256':digest(path),'layout':layout})
 zones=geometry(manifest['seed'],manifest['batch_zones'])
 rng=random.Random(manifest['seed']+1);single=[]
 from shapely.geometry import box,mapping
 for pattern in ['hot','scatter']:
  for i in range(manifest['single_queries_per_pattern']):
   x,y=rng.uniform(12,110),rng.uniform(12,110)
   if pattern=='scatter':x+=(i%2)*256;y+=((i//2)%2)*256
   single.append({'id':f'{pattern}{i:03d}','pattern':pattern,'geometry':mapping(box(x,y,x+rng.uniform(.2,8),y+rng.uniform(.2,8)))})
 data={'sources':sources,'zones':zones,'single_zones':single,'build_ms':(time.perf_counter()-started)*1000,'total_source_bytes':sum(x['bytes'] for x in sources),'redistribution':'These files are synthetic outputs of this benchmark. No external raster or application fixture is embedded.'}
 dump(out/'fixtures.json',data);return data

def fraction_window(p,height=512,width=512):
 import numpy as np
 import shapely
 from shapely.geometry import shape,box
 p=shape(p);xmin,ymin,xmax,ymax=p.bounds
 left=max(0,math.floor(xmin));right=min(width,math.ceil(xmax));top=max(0,math.floor(height-ymax));bottom=min(height,math.ceil(height-ymin))
 if right<=left or bottom<=top:return (0,0,0,0),np.zeros((0,0))
 yy,xx=np.mgrid[top:bottom,left:right]
 if p.equals(box(*p.bounds)):
  fx=np.maximum(0,np.minimum(xx+1,xmax)-np.maximum(xx,xmin));fy=np.maximum(0,np.minimum(height-yy,ymax)-np.maximum(height-yy-1,ymin));fraction=fx*fy
 else:fraction=shapely.area(shapely.intersection(p,shapely.box(xx,height-yy-1,xx+1,height-yy)))
 return (left,top,right,bottom),fraction

def oracle(out,fixtures):
 import numpy as np
 import rasterio
 rows={};started=time.perf_counter()
 zone_sets=[('batch',fixtures['zones']),('single',fixtures['single_zones'])]
 for spec in fixtures['sources']:
  with rasterio.open(out/spec['path']) as dataset:values=dataset.read().astype('float64')
  for group,zones in zone_sets:
   if group=='single' and spec['id']!='single':continue
   for z in zones:
    (left,top,right,bottom),fractions=fraction_window(z['geometry']);answer=[]
    for band in values:
     v=band[top:bottom,left:right];valid=(v!=-9999.) & np.isfinite(v) & (fractions>0);selected=v[valid];weights=fractions[valid]
     total=math.fsum(float(v*f) for v,f in zip(selected,weights));support=math.fsum(float(f) for f in weights)
     answer.append({'sum':total,'support':support,'mean':total/support if support else None,'min':float(selected.min()) if selected.size else None,'max':float(selected.max()) if selected.size else None,'valid_count':int(selected.size),'mass':math.fsum(abs(float(v*f)) for v,f in zip(selected,weights))})
    rows[(group,spec['id'],z['id'])]=answer
 return rows,(time.perf_counter()-started)*1000

def compare(actual,expected):
 differences=[]
 for a,e in zip(actual,expected):
  for field in FIELDS+(['valid_count'] if 'valid_count' in a else []):
   x,y=a.get(field),e[field]
   if x is None or y is None:
    if x!=y:differences.append({'statistic':field,'error':'null mismatch','actual':x,'expected':y})
    continue
   error=abs(x-y);tol=0. if field=='valid_count' else (1e-8+1e-10*(e['mass'] if field=='sum' else abs(y)))
   if error>tol:differences.append({'statistic':field,'absolute_error':error,'tolerance':tol,'actual':x,'expected':y})
 return differences

def native_band(value):return dict(zip(FIELDS,[value[x] for x in NATIVE]))|{'valid_count':value['valid_cell_count']}
def reference_band(value):
 # Upstream emits NaN for empty means/extrema. Map only those absent values
 # to the common JSON null representation; nonempty nonfinite results fail.
 if value['support']==0:
  for field in ['mean','min','max']:
   if value[field] is not None and not math.isfinite(value[field]):value[field]=None
 return value
def sink(rows):return len(json.dumps(rows,sort_keys=True,separators=(',',':'),allow_nan=False).encode())

def batch_native(out,fixtures,family,mode,policy,library):
 from skarve import Engine
 sources=[s for s in fixtures['sources'] if (s['id']=='dates' if family=='dates' else s['id'].startswith('file'))]
 started=time.perf_counter();engine=Engine(library);slices=[]
 try:
  for source in sources:
   if family=='dates':
    for first in range(0,source['bands'],20):
     key=f'dates{first}';engine.register_source({'location':str(out/source['path']),'bands':list(range(first,min(first+20,source['bands'])))},id=key)
     slices.extend({'id':f'date{band:02d}','source':key,'bands':[band-first],'time':f'2020-01-{band+1:02d}T00:00:00Z','variable':'synthetic_signal'} for band in range(first,min(first+20,source['bands'])))
   else:
    engine.register_source({'location':str(out/source['path'])},id=source['id']);slices.append({'id':source['id'],'source':source['id']})
  setup_ms=(time.perf_counter()-started)*1000;job={'zones':[{k:z[k] for k in ['id','version','geometry']} for z in fixtures['zones']],'slices':slices,'crs':'EPSG:3857','tile_edge':64,'geometry_layout':'auto','window_policy':policy,'output_mode':mode,'options':{'statistics':FIELDS+['count']}}
  rows=[];metrics={};query=time.perf_counter()
  for page in engine.batch_pages(job,max_rows=64,checkpoint_interval=2**30):
   rows.extend({'zone':r['zone_id'],'source':r['slice_id'],'bands':[native_band(b) for b in r['bands']]} for r in page['rows']);metrics=page['metrics']
  size=sink(rows);query_ms=(time.perf_counter()-query)*1000
 finally:engine.close()
 return {'elapsed_ms':(time.perf_counter()-started)*1000,'setup_ms':setup_ms,'query_ms':query_ms,'sink_bytes':size,'answers':rows,'metrics':metrics}

def batch_reference(out,fixtures,family,strategy):
 import exactextract
 import rasterio
 from exactextract.raster import RasterioRasterSource
 from exactextract.feature import JSONFeatureSource
 sources=[s for s in fixtures['sources'] if (s['id']=='dates' if family=='dates' else s['id'].startswith('file'))]
 started=time.perf_counter()
 with contextlib.ExitStack() as stack:
  stack.enter_context(rasterio.Env(GDAL_CACHEMAX=64<<20))
  readers=[]
  for source in sources:
   ds=stack.enter_context(rasterio.open(out/source['path']))
   readers.extend(RasterioRasterSource(ds,band+1,name=f'{source["id"]}_{band}') for band in range(source['bands']))
  features=JSONFeatureSource([{'type':'Feature','properties':{'zone_key':z['id']},'geometry':z['geometry']} for z in fixtures['zones']],srs_wkt=ds.crs.wkt)
  raw=exactextract.exact_extract(readers,features,UPSTREAM,strategy=strategy,include_cols=['zone_key'],max_cells_in_memory=1_000_000)
 rows=[]
 for feature in raw:
  value=feature['properties']
  for source in sources:
   bands=[reference_band(dict(zip(FIELDS,[value[f'{source["id"]}_{band}_{field}'] for field in UPSTREAM]))) for band in range(source['bands'])]
   if family=='dates':rows.extend({'zone':value['zone_key'],'source':f'date{band:02d}','bands':[b]} for band,b in enumerate(bands))
   else:rows.append({'zone':value['zone_key'],'source':source['id'],'bands':bands})
 size=sink(rows)
 return {'elapsed_ms':(time.perf_counter()-started)*1000,'sink_bytes':size,'answers':rows}

def single_runs(out,fixtures,manifest,library):
 from skarve import Engine
 import exactextract
 import rasterio
 from exactextract.raster import RasterioRasterSource
 from exactextract.feature import JSONFeatureSource
 source=next(s for s in fixtures['sources'] if s['id']=='single');records=[]
 for mode,pattern,budget in [('default','hot',0),('decoded-warm','hot',1<<20),('cache-overflow','scatter',1<<20)]:
  with Engine(library) as engine:
   engine.call({'op':'configure_file_cache','bytes':budget});started=time.perf_counter();engine.register_source({'location':str(out/source['path'])},id='s');registration=(time.perf_counter()-started)*1000
   zones=[z for z in fixtures['single_zones'] if z['pattern']==pattern]
   # One explicit unscored fill is charged separately for the warm lane.
   warm=None
   if mode=='decoded-warm':
    from shapely.affinity import translate
    from shapely.geometry import shape,mapping
    warm_geometry=mapping(translate(shape(zones[0]['geometry']),xoff=.03125,yoff=.0625))
    began=time.perf_counter();value=engine.measure_source(warm_geometry,'EPSG:3857',source='s',statistics=FIELDS);warm={'elapsed_ms':(time.perf_counter()-began)*1000,'streaming':value['streaming'],'geometry':warm_geometry}
   for zone in zones:
    began=time.perf_counter();value=engine.measure_source(zone['geometry'],'EPSG:3857',source='s',statistics=FIELDS);answer=[native_band(value['bands'][0])];size=sink(answer)
    records.append({'experiment':'single','system':'skarve','method':mode,'pattern':pattern,'query':zone['id'],'elapsed_ms':(time.perf_counter()-began)*1000,'source_registration_ms':registration,'warmup':warm,'sink_bytes':size,'answers':[{'zone':zone['id'],'source':'single','bands':answer}],'metrics':value['streaming'],'timing_ms':value['timing_ms']})
 for strategy in ['feature-sequential','raster-sequential']:
  with rasterio.Env(GDAL_CACHEMAX=64<<20),rasterio.open(out/source['path']) as ds:
   reader=RasterioRasterSource(ds,1)
   for zone in fixtures['single_zones']:
    began=time.perf_counter();raw=exactextract.exact_extract(reader,JSONFeatureSource([{'type':'Feature','properties':{},'geometry':zone['geometry']}],srs_wkt=ds.crs.wkt),UPSTREAM,strategy=strategy,max_cells_in_memory=1_000_000)[0]['properties'];answer=[dict(zip(FIELDS,[raw[f] for f in UPSTREAM]))];size=sink(answer)
    records.append({'experiment':'single','system':'exactextract','method':strategy,'pattern':zone['pattern'],'query':zone['id'],'elapsed_ms':(time.perf_counter()-began)*1000,'sink_bytes':size,'answers':[{'zone':zone['id'],'source':'single','bands':answer}]})
 return records

def preparation_runs(out,fixtures,library):
 from skarve import Engine
 source=next(s for s in fixtures['sources'] if s['id']=='single');records=[];setup={}
 with Engine(library) as engine:
  began=time.perf_counter();engine.register_source({'location':str(out/source['path'])},id='s');setup['source_registration_ms']=(time.perf_counter()-began)*1000
  for method in ['direct-source','original-summary-index']:
   options={}
   if method=='original-summary-index':
    index=out/'source-index';began=time.perf_counter();built=engine.prepare_source(index,source='s',tile_edge=64,boundary_source='original');setup['build_ms']=(time.perf_counter()-began)*1000
    setup['index_bytes']=sum(p.stat().st_size for p in index.rglob('*') if p.is_file());setup['normalized_values_bytes']=0
    began=time.perf_counter();engine.register_index(index,source='s',id='index');setup['index_registration_ms']=(time.perf_counter()-began)*1000;options={'index_handle':'index'}
   for zone in fixtures['zones']:
    began=time.perf_counter();value=engine.measure_source(zone['geometry'],'EPSG:3857',source='s',statistics=FIELDS,**options);answer=[native_band(value['bands'][0])];size=sink(answer)
    records.append({'experiment':'prepared','system':'skarve','method':method,'query':zone['id'],'elapsed_ms':(time.perf_counter()-began)*1000,'sink_bytes':size,'answers':[{'zone':zone['id'],'source':'single','bands':answer}]})
 return records,setup

def run(args):
 out=Path(args.output).resolve();manifest=load(out/'freeze.json');assert digest(core_path(args.library))==manifest['library_sha256'],'Installed native candidate changed after freeze';assert digest(__file__)==manifest['harness_sha256'],'Benchmark source changed after freeze'
 resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30));fixtures=make_fixtures(out,manifest);expected,oracle_ms=oracle(out,fixtures);records=[];errors=[];tasks=[]
 for round in range(manifest['rounds']):
  per=[]
  for family in ['dates','files']:
   per += [(round,family,'skarve',mode,policy) for mode in ['numeric','full'] for policy in ['fixed','source_layout']]
   per += [(round,family,'exactextract',strategy,None) for strategy in ['feature-sequential','raster-sequential']]
  random.Random(manifest['seed']+round+30).shuffle(per);tasks+=per
 for round,family,system,mode,policy in tasks:
  try:
   measured=batch_native(out,fixtures,family,mode,policy,args.library) if system=='skarve' else batch_reference(out,fixtures,family,mode)
   records.append({'experiment':'batch','family':family,'system':system,'method':policy+'-'+mode if policy else mode,'round':round}|measured)
  except Exception as error:errors.append({'experiment':'batch','family':family,'system':system,'method':str(policy)+'/'+mode,'round':round,'error':type(error).__name__+': '+str(error).replace(str(out),'<benchmark-output>')})
  print(json.dumps({'family':family,'system':system,'method':str(policy)+'/'+mode,'round':round,'records':len(records),'errors':len(errors)}),flush=True)
 try:records+=single_runs(out,fixtures,manifest,args.library)
 except Exception as error:errors.append({'experiment':'single','error':type(error).__name__+': '+str(error).replace(str(out),'<benchmark-output>')})
 preparation={}
 try:
  extra,preparation=preparation_runs(out,fixtures,args.library);records+=extra
 except Exception as error:errors.append({'experiment':'prepared','error':type(error).__name__+': '+str(error).replace(str(out),'<benchmark-output>')})
 validation=[]
 for record in records:
  failures=[];maxima=defaultdict(float);checks=0
  actual_keys=[(row['zone'],row['source']) for row in record['answers']]
  if record['experiment']=='batch':
   source_ids=[f'date{i:02d}' for i in range(manifest['date_slices'])] if record['family']=='dates' else [f'file{i}' for i in range(manifest['distinct_files'])]
   wanted_keys={(z['id'],source) for z in fixtures['zones'] for source in source_ids}
  else:wanted_keys={(record['query'],'single')}
  assert len(actual_keys)==len(wanted_keys) and set(actual_keys)==wanted_keys,'Missing or duplicate useful result rows'
  for row in record['answers']:
   group='single' if record['experiment']=='single' else 'batch';source=row['source']
   if source.startswith('date'):want=[expected[(group,'dates',row['zone'])][int(source[4:])]]
   else:want=expected[(group,source,row['zone'])]
   assert len(want)==len(row['bands'])
   differences=compare(row['bands'],want);failures.extend({'zone':row['zone'],'source':source}|d for d in differences)
   for actual,target in zip(row['bands'],want):
    checks+=len(FIELDS)+('valid_count' in actual)
    for field in FIELDS:
     if actual[field] is not None and target[field] is not None:maxima[field]=max(maxima[field],abs(actual[field]-target[field]))
  validation.append({'record':len(validation),'system':record['system'],'experiment':record['experiment'],'method':record['method'],'checks':checks,'strict_failures':failures,'max_absolute_error':dict(maxima),'scope':'Native strict correctness gate' if record['system']=='skarve' else 'External compatibility at native tolerance; discrepancies are retained, not interpreted as interchangeable semantics'})
 hashes={s['path']:digest(out/s['path']) for s in fixtures['sources']};assert all(hashes[s['path']]==s['sha256'] for s in fixtures['sources'])
 result={'schema':1,'freeze':manifest,'fixture_manifest_sha256':digest(out/'fixtures.json'),'oracle_ms':oracle_ms,'records':records,'validation':validation,'errors':errors,'native_gate_pass':not errors and all(not v['strict_failures'] for v in validation if v['system']=='skarve'),'source_files_unchanged':True,'max_process_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,'boundary':'Batch: public source registration/open, native/shared all-zone execution, result normalization, close and canonical sink. exactextract uses natural all-source all-feature calls. Native adds integer valid count to the five shared fields. Source adapters, fixture creation and independent oracle are reported separately. Single calls retain source handles; warmup/source setup is separate. GDAL cache is64MiB; native decoded cache adds explicit0/1MiB within the common2GiB process cap.','limitations':['One Linux host; no cold-storage or universal superiority claim.','Three batch repeats show spread, not a stable p95. Single patterns use distinct geometry.','GEOS cell intersections and fsum are an independent finite-precision oracle, not exact-real arithmetic. Thin rectangles use separable axis overlap.','No external reference is automatically eligible for strict execution.','No private application/source fixture or credential is required.']}
 result['optional_preparation']=preparation
 dump(out/'results.json.gz',result)
 fields=['experiment','family','pattern','system','method','round','query','elapsed_ms','setup_ms','query_ms','sink_bytes']
 with (out/'timings.csv').open('w',newline='') as stream:
  writer=csv.DictWriter(stream,fieldnames=fields,lineterminator='\n');writer.writeheader();writer.writerows({k:r.get(k) for k in fields} for r in records)
 print(json.dumps({'records':len(records),'errors':len(errors),'native_gate_pass':result['native_gate_pass'],'max_process_rss_bytes':result['max_process_rss_bytes']}))
 assert result['native_gate_pass']

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('stage',choices=['freeze','run']);p.add_argument('--output',required=True);p.add_argument('--library');p.add_argument('--expected-library-sha256');p.add_argument('--seed',type=int);p.add_argument('--quick',action='store_true');args=p.parse_args()
 if args.stage=='freeze':freeze(args)
 else:run(args)
if __name__=='__main__':main()
