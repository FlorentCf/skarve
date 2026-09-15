#!/usr/bin/env python3
"""Compile serving objects and existing COG summaries through installed Skarve.

Conversion receives no benchmark polygons. Source identities are explicit content
manifests so the same prepared COG index can be opened against controlled HTTP.
"""
from __future__ import annotations
import argparse, hashlib, json, os, resource, struct, sys, time, zlib
from pathlib import Path

for name in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS','GDAL_NUM_THREADS'):os.environ[name]='1'
os.environ['GDAL_CACHEMAX']='64';os.environ['PROJ_NETWORK']='OFF'
ROOT=Path(__file__).resolve().parents[2]

def digest(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--fixtures',type=Path,nargs='+',required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--library',type=Path,required=True);p.add_argument('--checkout-bindings',action='store_true');p.add_argument('--datasets');p.add_argument('--chunk-edge',type=int,default=256);p.add_argument('--index-edges',default='64,256');p.add_argument('--reuse-conversions',type=Path);p.add_argument('--band-group',type=int,default=4);p.add_argument('--codec',choices=['deflate','none'],default='deflate');p.add_argument('--predictor',choices=['none','byte_delta_v1'],default='none');p.add_argument('--payload-layout',choices=['band','row_group_v1'],default='band');p.add_argument('--skip-equivalent-cog',action='store_true');p.add_argument('--extra-index-variants',default='');p.add_argument('--compile-working-bytes',type=int,default=128<<20);args=p.parse_args()
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    from skarve import Skarve
    assert not args.output.exists(),'Fresh preparation destination required';args.output.mkdir(parents=True)
    selected=set(args.datasets.split(',')) if args.datasets else None;sources=[];start=time.perf_counter()
    for manifest_path in args.fixtures:
        fixture=json.loads(manifest_path.read_text());directory=manifest_path.parent
        for source in fixture['sources']:
            # Older development fixture manifests called the cropped WorldPop
            # row worldpop1 too. Give it an explicit unique identity before
            # combining it with the complete native-raster manifest.
            window=source.get('origin',{}).get('crop_window_xywh');original=source.get('origin',{}).get('original_grid')
            if source['id']=='worldpop1' and window and window[2:]!=original:source={**source,'id':'worldpop1-crop'}
            if selected and source['id'] not in selected:continue
            assert all(s['id']!=source['id'] for s in sources),'Duplicate logical dataset ID'
            # Keep source files in their original task directory. Store derived
            # paths relative to that directory without copying their source data.
            record={**source,'directory':str(directory),'crs':'EPSG:3857' if source['id'].startswith('analytical') else 'EPSG:4326','variants':{k:dict(v) for k,v in source['variants'].items()}}
            index_variants=['cog256']+[v for v in args.extra_index_variants.split(',') if v and v!='cog256']
            for variant in ['ordinary']+index_variants:
                item=source['variants'][variant];path=directory/item['path'];assert digest(path)==item['sha256']
                spec={'location':str(path),'identity':{'sha256':item['sha256'],'byte_length':item['bytes'],'policy':'trusted_manifest'}}
                if variant=='ordinary' or variant=='cog256' and not args.skip_equivalent_cog:
                    previous=args.reuse_conversions/f'{source["id"]}-{variant}-conversion.json' if args.reuse_conversions else None
                    if previous and previous.exists():
                        built=json.loads(previous.read_text());target=directory/built['path']
                        assert target.stat().st_size==built['bytes'] and digest(target)==built['sha256'],'Previous conversion changed'
                        identity=built['conversion']['source_identity']
                        assert identity['content_sha256']==item['sha256'] and identity['byte_length']==item['bytes'],'Previous conversion belongs to another source'
                        with target.open('rb') as stored:header=stored.read(16384)
                        encoded=struct.unpack_from('<I',header,24)[0];metadata=json.loads(zlib.decompress(header[64:64+encoded]))
                        assert metadata['chunk_edge']==args.chunk_edge and metadata['band_group']==min(args.band_group,len(source['logical']['bands'])) and metadata['codec']==args.codec and metadata['summaries'],'Previous conversion options differ'
                        assert metadata.get('predictor','none')==args.predictor,'Previous conversion predictor differs'
                        assert metadata.get('payload_layout','band')==args.payload_layout,'Previous conversion payload layout differs'
                        assert [b['original_band_index'] for b in metadata['raw_metadata']['bands']]==list(range(len(source['logical']['bands']))),'Previous conversion band selection differs'
                        record['variants']['skv' if variant=='ordinary' else 'skv_from_cog']={**built,'reused_conversion_receipt':str(previous)}
                    else:
                        built=compile_variant(args,source,variant,directory,spec,Skarve)
                        record['variants']['skv' if variant=='ordinary' else 'skv_from_cog']=built
                if variant in index_variants:
                  for index_edge in map(int,args.index_edges.split(',')):
                    assert index_edge in (16,64,256),'Existing index contract'
                    groups=[];began_all=time.perf_counter()
                    for first in range(0,len(source['logical']['bands']),20):
                        mapping=list(range(first,min(first+20,len(source['logical']['bands']))));group_spec={**spec,'bands':mapping}
                        index=args.output/f'{source["id"]}-{variant}-index{index_edge}-{first:02d}';began=time.perf_counter()
                        with Skarve(args.library) as engine:
                            with engine.infuse(group_spec) as reader:
                                opened=time.perf_counter()-began
                                prepared=reader.ward(str(index),boundary_source='original',tile_edge=index_edge,summary_backend='hierarchy')
                        elapsed=time.perf_counter()-began
                        groups.append({'source_bands':mapping,'path':os.path.relpath(index/'summary.rsi',directory),'build_id':prepared['build_id'],'bytes':(index/'summary.rsi').stat().st_size,'sha256':digest(index/'summary.rsi'),'preparation_wall_seconds':elapsed,'registration_wall_seconds':opened,'preparation':prepared})
                        (args.output/f'{source["id"]}-{variant}-index{index_edge}-{first:02d}.json').write_text(json.dumps(groups[-1],indent=2,allow_nan=False)+'\n')
                    record['variants'][variant][f'index{index_edge}']={'groups':groups,'tile_edge':index_edge,'bytes':sum(g['bytes'] for g in groups),'preparation_wall_seconds':time.perf_counter()-began_all,'scope':'Existing at-most20-band indexes; selected groups use one shared cleave job, preserving the original band order.'}
            sources.append(record)
            print(json.dumps({'source':source['id'],'skv_bytes':record['variants']['skv']['bytes']}),flush=True)
    receipt={'schema':'skarve_skv_prepared_fixtures_v1','library_sha256':digest(args.library),'sources':sources,'wall_seconds':time.perf_counter()-start,'peak_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,'address_space_limit_bytes':2<<30}
    (args.output/'prepared.json').write_text(json.dumps(receipt,indent=2,allow_nan=False)+'\n')

def compile_variant(args,source,variant,directory,spec,Skarve):
    target=args.output/f'{source["id"]}-{variant}.skv';began=time.perf_counter()
    with Skarve(args.library) as engine:
        with engine.infuse(spec) as reader:
            opened=time.perf_counter()-began
            optional={'predictor':args.predictor} if args.predictor!='none' else {}
            if args.payload_layout!='band':optional['payload_layout']=args.payload_layout
            receipt=reader.compile(str(target),chunk_edge=args.chunk_edge,band_group=min(args.band_group,len(source['logical']['bands'])),codec=args.codec,compression_level=3,summaries=True,working_bytes=args.compile_working_bytes,max_output_bytes=2<<30,**optional)
    elapsed=time.perf_counter()-began
    built={'path':os.path.relpath(target,directory),'sha256':digest(target),'bytes':target.stat().st_size,'conversion_wall_seconds':elapsed,'registration_wall_seconds':opened,'conversion':receipt}
    (args.output/f'{source["id"]}-{variant}-conversion.json').write_text(json.dumps(built,indent=2,allow_nan=False)+'\n')
    return built

if __name__=='__main__':main()
