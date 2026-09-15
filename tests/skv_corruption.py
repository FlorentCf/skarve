#!/usr/bin/env python3
"""Repair checksums around malformed SKV structures to reach semantic parsing.

Run against a small generated SKV. Each case must fail both the independent
decoder and a complete raw native query. No production/source file is modified.
"""
from __future__ import annotations
import argparse, json, struct, sys, zlib
from pathlib import Path
from blake3 import blake3
from skv_format_oracle import Oracle, HEADER, PAGE

def repair_header(data):data[HEADER-32:HEADER]=blake3(data[:HEADER-32]).digest()

def change_metadata(data,change):
    length=struct.unpack_from('<I',data,24)[0];metadata=json.loads(zlib.decompress(data[64:64+length]))
    change(metadata)
    decoded=json.dumps(metadata,separators=(',',':')).encode();encoded=zlib.compress(decoded)
    assert len(encoded)<=HEADER-96
    struct.pack_into('<II',data,24,len(encoded),len(decoded));data[64:HEADER-32]=bytes(HEADER-96);data[64:64+len(encoded)]=encoded;repair_header(data)

def repair_page(data,page_id=0):
    offset=HEADER+page_id*PAGE;data[offset+PAGE-32:offset+PAGE]=blake3(data[offset:offset+PAGE-32]).digest()
    length=struct.unpack_from('<I',data,24)[0];metadata=json.loads(zlib.decompress(data[64:64+length]));pages=struct.unpack_from('<Q',data,48)[0]
    metadata['directory_digest']=blake3(data[HEADER:HEADER+PAGE*pages]).hexdigest()
    encoded=zlib.compress(json.dumps(metadata,separators=(',',':')).encode());decoded=zlib.decompress(encoded)
    assert len(encoded)<=HEADER-96
    struct.pack_into('<II',data,24,len(encoded),len(decoded));data[64:HEADER-32]=bytes(HEADER-96);data[64:64+len(encoded)]=encoded;repair_header(data)

def cases(original):
    mutations=[]
    metadata_length=struct.unpack_from('<I',original,24)[0]
    metadata=json.loads(zlib.decompress(original[64:64+metadata_length]))
    grouped=metadata.get('payload_layout','band')=='row_group_v1'
    def add(name,change,repair=None):
        data=bytearray(original);change(data)
        if repair:repair(data)
        mutations.append((name,data))
    def u32(offset,value):return lambda data:struct.pack_into('<I',data,offset,value)
    def u64(offset,value):return lambda data:struct.pack_into('<Q',data,offset,value)
    add('bootstrap_checksum',u32(8,2))
    add('unknown_major_version',u32(8,2),repair_header)
    add('unknown_header_flag',u32(12,0x80000000),repair_header)
    add('predictor_flag_mismatch',u32(12,struct.unpack_from('<I',original,12)[0]^2),repair_header)
    add('unknown_predictor',lambda data:change_metadata(data,lambda m:m.update(predictor='unknown_v1')))
    add('unknown_payload_layout',lambda data:change_metadata(data,lambda m:m.update(payload_layout='unknown_v1')))
    add('group_flag_mismatch',u32(12,struct.unpack_from('<I',original,12)[0]^8),repair_header)
    add('predictor_metadata_mismatch',lambda data:change_metadata(data,lambda m:m.update(predictor='none' if m.get('predictor','none')=='byte_delta_v1' else 'byte_delta_v1')))
    add('unknown_metadata_codec',u32(20,99),repair_header)
    add('decoded_metadata_overflow',u32(28,0xffffffff),repair_header)
    add('object_length_overflow',u64(32,(1<<64)-1),repair_header)
    add('directory_page_count_overflow',u64(48,(1<<64)-1),repair_header)
    add('bootstrap_padding',lambda data:data.__setitem__(HEADER-33,1),repair_header)
    record=HEADER+16
    add('record_identity',u64(record+104,99),repair_page)
    add('unknown_payload_codec',u32(record+24,99),repair_page)
    add('decoded_payload_overflow',u32(record+12,0xffffffff),repair_page)
    add('offset_length_overflow',u64(record,(1<<64)-1),repair_page)
    add('record_reserved',u32(record+124,1),repair_page)
    add('payload_checksum',lambda data:data.__setitem__(struct.unpack_from('<Q',data,record)[0],data[struct.unpack_from('<Q',data,record)[0]]^128))
    # Group members intentionally alias one payload. Mutate a different
    # physical group instead of recording a no-op as a corruption attempt.
    edge=metadata['chunk_edge'];grid=metadata['grid'];band_count=len(metadata['raw_metadata']['bands'])
    leaf_records=((grid['width']+edge-1)//edge)*((grid['height']+edge-1)//edge)*band_count
    first_offset=struct.unpack_from('<Q',original,record)[0]
    for record_id in range(1,leaf_records):
        page_id=record_id//64;at=HEADER+page_id*PAGE+16+(record_id%64)*128
        if struct.unpack_from('<Q',original,at)[0]!=first_offset:
            add('payload_overlap',u64(at,first_offset),lambda data,page_id=page_id:repair_page(data,page_id))
            break
    if grouped:
        members=struct.unpack_from('<I',original,record+120)[0]
        add('group_leader_forgery',u64(record+112,1),repair_page)
        add('group_count_forgery',u32(record+120,members+1),repair_page)
        if members>1:
            alias=record+128
            add('group_alias_offset',u64(alias,first_offset+1),repair_page)
            add('group_alias_checksum',lambda data:data.__setitem__(alias+32,data[alias+32]^1),repair_page)
    for size in (16,HEADER-1,HEADER+PAGE-1,len(original)-1):mutations.append((f'truncated_{size}',bytearray(original[:size])))
    return mutations

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--fixture',type=Path,required=True);p.add_argument('--library',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--checkout-bindings',action='store_true');args=p.parse_args()
    if args.checkout_bindings:sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'bindings/python'))
    from skarve import Skarve
    assert args.fixture.stat().st_size<=16<<20,'Use a small generated corruption fixture'
    assert not args.output.exists(),'No overwrite';args.output.mkdir(parents=True)
    with Oracle(args.fixture) as oracle:
        oracle.verify_payloads();bands=list(range(len(oracle.bands)));grid=oracle.metadata['grid'];transform=grid['transform'];x,y=transform[0],transform[3];right=x+grid['width']*transform[1];bottom=y+grid['height']*transform[5]
        zone={'type':'Polygon','coordinates':[[[x,bottom],[right,bottom],[right,y],[x,y],[x,bottom]]]}
    results=[]
    for name,data in cases(args.fixture.read_bytes()):
        path=args.output/f'{name}.skv';path.write_bytes(data);row={'case':name}
        try:
            with Oracle(path) as decoder:decoder.verify_payloads()
            row['oracle_rejected']=False
        except (ValueError,KeyError,TypeError,struct.error,zlib.error) as error:row.update({'oracle_rejected':True,'oracle_error':str(error)})
        try:
            with Skarve(args.library) as engine:
                with engine.infuse({'location':str(path),'use_summaries':False}) as source:source.carve(zone,crs=grid['crs'],bands=bands,metrics=['sum','support','mean','min','max'])
            row['native_rejected']=False
        except Exception as error:row.update({'native_rejected':True,'native_error':str(error)})
        results.append(row)
    receipt={'schema':'skarve_skv_corruption_v1','passed':all(r['oracle_rejected'] and r['native_rejected'] for r in results),'cases':results}
    (args.output/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n');print(json.dumps(receipt,indent=2))
    return 0 if receipt['passed'] else 1

if __name__=='__main__':raise SystemExit(main())
