#!/usr/bin/env python3
"""Combine immutable preparation receipts without copying or converting pixels."""
import argparse,hashlib,json
from pathlib import Path

def digest(path):
    with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--prepared',type=Path,nargs='+',required=True);p.add_argument('--output',type=Path,required=True)
    args=p.parse_args();assert not args.output.exists(),'Fresh combined manifest required'
    records=[];components=[]
    for path in args.prepared:
        receipt=json.loads(path.read_text());components.append({'path':str(path.resolve()),'sha256':digest(path),'preparation_library_sha256':receipt['library_sha256']})
        for source in receipt['sources']:
            assert source['id'] not in {row['id'] for row in records},'Duplicate logical dataset'
            if source['id'].startswith('analytical'):
                source['data_scope']={'kind':'independently_authored_analytical_fixture','distinct_bands':len(source['logical']['bands']),'private_pixels':False,'distribution':'Reproducible generator; no private source data. Publication remains subject to the project owner decision.'}
            elif source['id']=='real-age36':
                source['data_scope']={'kind':'authorized_existing_private_native_crop','native_window_xywh':[2048,512,512,512],'original_native_grid':[4634,2408],'distinct_bands':36,'complete_country':False,'distribution':'Original and derived pixels remain private. Only bounded benchmark receipts are checkpointed.'}
            elif source['id']=='worldpop1':
                source['data_scope']={'kind':'authorized_existing_complete_worldpop_source','native_grid':[9722,7019],'native_samples':9722*7019,'cropped_serving_object':False,'distribution':'Original and derived pixels remain private during this programme; historical attribution and rights receipts are preserved.'}
            else:source['data_scope']={'kind':'unclassified','distribution':'Do not infer distribution rights from fixture availability.'}
            records.append(source)
    assert records
    combined={'schema':'skarve_skv_prepared_fixtures_v1','components':components,'sources':records,
        'scope':'All original source/preparation receipts retained in place; source/index identities verified by the programme before execution. Preparation library revisions can differ from the frozen query reader without changing file bytes.'}
    args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(combined,indent=2,allow_nan=False)+'\n')
    print(json.dumps({'datasets':[row['id'] for row in records],'output':str(args.output)}))

if __name__=='__main__':main()
