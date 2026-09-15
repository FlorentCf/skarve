#!/usr/bin/env python3
"""Prove source-owned summaries skip real fully covered SKV payload ranges."""
import argparse, hashlib, json, sys
from pathlib import Path
from skv_format_oracle import Oracle
ROOT=Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT/'benchmarks/skv-v0'));sys.path.insert(0,str(ROOT/'benchmarks/beta2'))
from range_server import served
from common import differences, native_bands

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--file',type=Path,required=True);p.add_argument('--library',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--checkout-bindings',action='store_true');p.add_argument('--exactextract',action='store_true');p.add_argument('--exactextract-strategy',choices=['feature-sequential','raster-sequential'],default='feature-sequential');args=p.parse_args()
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings/python'))
    from skarve import Skarve
    with Oracle(args.file) as oracle:
        grid=oracle.metadata['grid'];edge=oracle.metadata['chunk_edge'];bands=list(range(len(oracle.bands)));protected=[]
        assert grid['width']>=4*edge and grid['height']>=4*edge,'Use a fixture with multiple fully covered interior chunks'
        left=17.125;top=17.375;right=grid['width']-17.625;bottom=grid['height']-17.875
        for record in oracle.records:
            if not record.flags&1:continue
            x,y,width,height=oracle.chunk(record.id//len(bands))
            if left<=x and top<=y and x+width<=right and y+height<=bottom:protected.append(('source.skv',record.offset,record.offset+record.encoded-1))
        assert protected
        protected_leaf_records=len(protected)
        # Row-group aliases are separate logical bands sharing one physical
        # payload. Deny/count each interval once; legacy files remain unchanged.
        protected=sorted(set(protected))
        tx,dx,_,ty,_,dy=grid['transform']
        zone={'type':'Polygon','coordinates':[[[tx+left*dx,ty+bottom*dy],[tx+right*dx,ty+bottom*dy],[tx+right*dx,ty+top*dy],[tx+left*dx,ty+top*dy],[tx+left*dx,ty+bottom*dy]]]}
    sha=hashlib.sha256(args.file.read_bytes()).hexdigest();length=args.file.stat().st_size
    observations=[];controls={}
    for mode,deny,backend in [('raw_control',False,'native'),('summary_proof',True,'native'),('raw_rejected',True,'native')]+([('upstream_raw',False,'exactextract')] if args.exactextract else []):
        with served({'source.skv':args.file},forbidden=protected if deny else ()) as server:
            spec={'location':server.url('source.skv'),'use_summaries':mode=='summary_proof','http':{'allow_http':True,'max_requests':8192,'max_download_bytes':768<<20,'max_range_bytes':4<<20},'identity':{'sha256':sha,'byte_length':length,'policy':'trusted_manifest','etag':'"'+sha+'"'}}
            row={'mode':mode,'backend':backend}
            if backend=='exactextract':row['strategy']=args.exactextract_strategy
            try:
                with Skarve(args.library) as engine:
                    with engine.infuse(spec) as source:
                        result=source.carve(zone,crs=grid['crs'],bands=bands,metrics=['sum','support','mean','min','max'],backend=backend,**({'numerical_policy':'exactextract_fractional_v030','backend_options':{'strategy':args.exactextract_strategy,'window_bytes':256<<20,'max_cells_in_memory':262144}} if backend=='exactextract' else {}))
                row.update({'succeeded':True,'answer':native_bands(result,bands),'metrics':{key:result[key] for key in ('work','streaming','source_access','provenance') if key in result}})
            except Exception as error:row.update({'succeeded':False,'error':str(error)})
            row['requests']=server.snapshot();row['raw_protected_rejections']=sum(r['status']==403 for r in row['requests'])
            observations.append(row)
    lookup={r['mode']:r for r in observations};errors=[]
    for mode in ['raw_control','summary_proof']+(['upstream_raw'] if args.exactextract else []):
        if not lookup[mode]['succeeded']:errors.append(mode+': '+lookup[mode].get('error','failed'))
    if lookup['raw_rejected']['succeeded'] or lookup['raw_rejected']['raw_protected_rejections']==0:errors.append('Raw rejection control did not encounter a protected payload')
    if lookup['summary_proof']['succeeded'] and lookup['raw_control']['succeeded']:errors.extend(differences(lookup['summary_proof']['answer'],lookup['raw_control']['answer'],strict=True))
    if lookup['summary_proof']['raw_protected_rejections']:errors.append('Summary path attempted protected raw payload')
    receipt={'schema':'skarve_skv_protected_ranges_v1','passed':not errors,'errors':errors,'file_sha256':sha,
        'protected_payloads':len(protected),'unique_protected_payloads':len(protected),'protected_leaf_records':protected_leaf_records,
        'protected_encoded_bytes':sum(b-a+1 for _,a,b in protected),'bands':bands,'observations':observations,
        'scope':'Native summary/raw use the same serving object and complete source-owned API. Range denial and encoded bytes count unique independently parsed physical payload intervals. Logical leaf records additionally count grouped band aliases. No latency claim from this mechanism proof.'}
    assert not args.output.exists();args.output.write_text(json.dumps(receipt,indent=2,allow_nan=False)+'\n');print(json.dumps({'passed':not errors,'errors':errors,'protected_payloads':len(protected),'output':str(args.output)}))
    if errors:raise SystemExit(1)

if __name__=='__main__':main()
