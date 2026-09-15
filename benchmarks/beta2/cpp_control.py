#!/usr/bin/env python3
"""Same-build C++ control, explicitly separate from source-owning file timings."""
from __future__ import annotations
import argparse
import json
import math
from pathlib import Path
import resource
import struct
import subprocess
import time

from common import FIELDS, differences, digest, normalized_file, read_json, write_json


def make_job(path, folder, sources, zones, strategy):
    import numpy as np
    from shapely.geometry import shape
    started=time.perf_counter();normalized=[normalized_file(folder/s['path']) for s in sources]
    first=normalized[0]
    assert all(n['transform']==first['transform'] and n['extent']==first['extent'] for n in normalized)
    height,width=first['values'][0].shape
    bands=sum(len(n['values']) for n in normalized)
    geometries=[shape(z['geometry']).wkb for z in zones]
    with path.open('xb') as output:
        output.write(b'SKAREE01')
        output.write(struct.pack('<6Q',strategy,width,height,bands,len(zones),1_000_000))
        output.write(struct.pack('<6d',*first['extent'],first['transform'][1],-first['transform'][5]))
        for n in normalized:
            for values,valid in zip(n['values'],n['valid']):
                output.write(np.asarray(values,dtype='<f8').tobytes())
                output.write(np.asarray(valid,dtype='u1').tobytes())
        for geometry in geometries:
            output.write(struct.pack('<Q',len(geometry)));output.write(geometry)
    return {'preparation_ms':(time.perf_counter()-started)*1000,'job_bytes':path.stat().st_size,
            'job_sha256':digest(path),'zones':len(zones),'bands':bands,
            'normalized_snapshot_bytes':sum(n['decoded_bytes'] for n in normalized),
            'source_hashes':{s['path']:digest(folder/s['path']) for s in sources}}


def canonical(run,zones,bands):
    assert len(run['values'])==len(run['defined'])==zones*bands*5
    assert all(value in (0,1) for value in run['defined']), 'Invalid defined-field byte'
    assert all(math.isfinite(value) for value,defined in zip(run['values'],run['defined']) if defined), 'Defined nonfinite control result'
    output=[]
    for z in range(zones):
        values=[]
        for b in range(bands):
            values.append({field:float(run['values'][(z*bands+b)*5+k]) if run['defined'][(z*bands+b)*5+k] else None for k,field in enumerate(FIELDS)})
        output.append(values)
    return output


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--folder',type=Path,required=True)
    parser.add_argument('--control',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--native-build-receipt',type=Path,required=True,
                        help='Receipt from the optional build that produced the measured native library')
    parser.add_argument('--rounds',type=int,default=3)
    parser.add_argument('--zones',type=int,default=64)
    args=parser.parse_args();assert not args.output.exists() and 1<=args.rounds<=3
    args.output.mkdir(parents=True);resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    freeze=read_json(args.folder/'freeze.json');fixtures=read_json(args.folder/'extended-fixtures.json')
    build=read_json(args.control.parent/'build-receipt.json')
    native_build=read_json(args.native_build_receipt)
    same_keys=('library_sha256','bridge_object_sha256','geos_sha256')
    assert all(build[key]==native_build[key] for key in same_keys), 'C++ control is not the same built upstream/bridge/GEOS'
    same_build={'matched_hashes':{key:build[key] for key in same_keys},
                'control_build_receipt_sha256':digest(args.control.parent/'build-receipt.json'),
                'native_build_receipt_sha256':digest(args.native_build_receipt),
                'upstream':build['upstream']}
    assert len(fixtures['zones'])>=args.zones
    before=digest(args.control);records=[];comparisons=[];errors=[];jobs=[]
    for family in ('dates','files'):
        sources=[s for s in fixtures['sources'] if (s['id']=='dates' if family=='dates' else s['id'].startswith('file'))]
        for strategy in (0,1):
            job=args.output/(family+'-'+str(strategy)+'.bin')
            prepared=make_job(job,args.folder,sources,fixtures['zones'][:args.zones],strategy)
            jobs.append({'family':family,'strategy':strategy,**prepared})
            for round in range(args.rounds):
                paired={}
                for mode in ('upstream','bridge'):
                    start=time.perf_counter()
                    try:
                        proc=subprocess.run([str(args.control),mode,str(job),'2'],capture_output=True,text=True,timeout=60)
                        elapsed=(time.perf_counter()-start)*1000
                        if proc.returncode:
                            errors.append({'family':family,'strategy':strategy,'round':round,'mode':mode,
                                'error':'C++ control process failed','exit_code':proc.returncode,
                                'stdout':proc.stdout,'stderr':proc.stderr,'process_wall_ms':elapsed})
                            continue
                        data=json.loads(proc.stdout)
                        assert data['schema']==1 and len(data['runs'])==2
                        assert data['mode']==mode and data['upstream_version']=='0.3.0'
                        for run in data['runs']:canonical(run,args.zones,prepared['bands'])
                        paired[mode]=data
                        records.append({'family':family,'strategy':strategy,'round':round,'mode':mode,
                                        'process_wall_ms':elapsed,'result':data})
                    except Exception as error:errors.append({'family':family,'strategy':strategy,'round':round,'mode':mode,'error':str(error)})
                if set(paired)=={'upstream','bridge'}:
                    mismatch=[];checks=0
                    for actual,control in zip(paired['bridge']['runs'],paired['upstream']['runs']):
                        a=canonical(actual,args.zones,prepared['bands']);b=canonical(control,args.zones,prepared['bands'])
                        for z,(aa,bb) in enumerate(zip(a,b)):
                            mismatch.extend({'zone':z,**e} for e in differences(aa,bb,strict=False));checks+=prepared['bands']*5
                    comparisons.append({'family':family,'strategy':strategy,'round':round,'passed':not mismatch,'checks':checks,'differences':mismatch})
    assert digest(args.control)==before
    expected=2*2*args.rounds
    result={'schema':'skarve_beta2_cpp_control_v1','passed':not errors and len(comparisons)==expected and all(x['passed'] for x in comparisons),
            'control_sha256':before,'harness_sha256':digest(__file__),'programme_freeze_sha256':digest(args.folder/'freeze.json'),
            'same_build_verification':same_build,
            'native_library_sha256':freeze['library_sha256'],'jobs':jobs,'records':records,'comparisons':comparisons,'errors':errors,
            'peak_parent_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,
            'scope':'Same-build independent upstream C++ ArraySource/MapWriter versus the typed bridge ABI over identical pre-normalized snapshots; two calls per process. No GDAL file source, native session, cache identity guard or installed binding in either timed C++ call.',
            'timing':'complete_ns includes processor/source/geometry/operations/output conversion/destruction, excludes shared binary-job load and JSON stdout. The processor and callback timers are nested. process_wall_ms includes executable startup, load and JSON transfer. Snapshot decode/WKB/job creation is separately recorded.',
            'limitations':['Control ArraySource returns views; bridge callbacks copy bounded windows. Their measured difference includes that deliberate ownership cost.',
                'This array diagnostic is not divided into original-file installed timings and called pure bridge overhead.',
                'The independent Python upstream calibration and full operation programme supply separate correctness/source coverage.']}
    write_json(args.output/'results.json',result)
    print(json.dumps({'passed':result['passed'],'records':len(records),'comparisons':len(comparisons),'errors':len(errors)}))
    return 0 if result['passed'] else 1


if __name__=='__main__':raise SystemExit(main())
