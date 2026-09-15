#!/usr/bin/env python3
"""One predeclared SKV option ablation, preserving existing COG/index controls.

Only the selected SKV object is rebuilt. Previous objects and preparation
receipts remain immutable; each source/index SHA is checked before reuse.
"""
from __future__ import annotations
import argparse,json,resource,sys,time
from pathlib import Path
from prepare import ROOT,compile_variant,digest

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--prepared',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--library',type=Path,required=True);p.add_argument('--checkout-bindings',action='store_true')
    p.add_argument('--datasets');p.add_argument('--chunk-edge',type=int,default=128)
    p.add_argument('--band-group',type=int,default=4);p.add_argument('--codec',choices=['deflate','none'],default='deflate')
    p.add_argument('--predictor',choices=['none','byte_delta_v1'],default='none')
    p.add_argument('--source-variant',choices=['ordinary','cog256','cogband128'],default='ordinary')
    p.add_argument('--compile-working-bytes',type=int,default=128<<20);args=p.parse_args()
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    else:
        from raster_engine_lab import resolve_library
        assert Path(resolve_library()).resolve()==args.library.resolve(),'Installed conversion must use its installed library'
    from skarve import Skarve
    assert not args.output.exists(),'Fresh ablation destination required';args.output.mkdir(parents=True)
    previous=json.loads(args.prepared.read_text());selected=set(args.datasets.split(',')) if args.datasets else None
    records=[];verified=set();setup=time.perf_counter();hashed=0
    for source in previous['sources']:
        if selected and source['id'] not in selected:continue
        directory=Path(source['directory'])
        for variant in source['variants'].values():
            for item in [variant]+[group for key,index in variant.items() if key.startswith('index') and isinstance(index,dict) for group in index['groups']]:
                path=(directory/item['path']).resolve()
                if path in verified:continue
                assert path.stat().st_size==item['bytes'] and digest(path)==item['sha256'],'Previous source/index object changed'
                verified.add(path);hashed+=path.stat().st_size
        records.append(source)
    assert records
    setup_receipt={'seconds':time.perf_counter()-setup,'bytes_hashed':hashed,'objects':len(verified),'scope':'Integrity setup outside conversion timer, preserving previous controls; can warm OS cache.'}
    began=time.perf_counter()
    for source in records:
        directory=Path(source['directory']);original=source['variants'][args.source_variant];path=directory/original['path']
        spec={'location':str(path),'identity':{'sha256':original['sha256'],'byte_length':original['bytes'],'policy':'trusted_manifest'}}
        built=compile_variant(args,source,args.source_variant,directory,spec,Skarve)
        source['variants']['skv']={**built,'previous_preparation_sha256':digest(args.prepared)}
        print(json.dumps({'dataset':source['id'],'skv_bytes':built['bytes'],'conversion_seconds':built['conversion_wall_seconds'],'predictor':args.predictor,'band_group':args.band_group}),flush=True)
    receipt={'schema':'skarve_skv_prepared_fixtures_v1','library_sha256':digest(args.library),'sources':records,
        'wall_seconds':time.perf_counter()-began,'peak_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,
        'address_space_limit_bytes':2<<30,'previous_preparation_sha256':digest(args.prepared),'fixture_integrity_setup':setup_receipt}
    (args.output/'prepared.json').write_text(json.dumps(receipt,indent=2,allow_nan=False)+'\n')

if __name__=='__main__':main()
