#!/usr/bin/env python3
"""Freeze all fourteen declared quadrants before running the bounded672-job matrix.

The installed runtime and geometry seed require an explicit acceptance receipt.
This driver keeps failed controls and delegates all numerical/timing contracts
to programme.py. It never prepares or changes raster/index objects.
"""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess
import sys

HERE=Path(__file__).resolve().parent
DATASETS={'worldpop1':'AB','real-age36':'ABCD','analytical36':'ABCD','analytical40-1025x1031':'ABCD'}


def digest(path):
    with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()


def write(path,value):
    assert not path.exists(),'Use a fresh immutable evidence destination'
    path.write_text(json.dumps(value,indent=2)+'\n')


def recipe(dataset,case,regime):
    native='native-cogband128';indexed=['native-cogband-index64','native-cogband-index']
    upstream='upstream-cogband'
    if dataset=='real-age36' and case in 'CD':
        native='native-cog128'
        indexed=['native-cogband-index64','native-cogband-index' if regime=='local' else 'native-cog-index']
        upstream='upstream-cog128' if case=='C' else 'upstream-cog'
    strategy='feature' if case in 'AC' else 'raster'
    reference=f'{upstream}-{strategy}'
    lanes=['native-ordinary',native,*indexed,'native-skv-raw','native-skv-summary',f'ee-skv-{strategy}',reference]
    assert len(set(lanes))==8
    return lanes,native,reference,strategy


def freeze(args):
    assert not args.output.exists()
    accepted=json.loads(args.acceptance.read_text())
    assert accepted['accepted_for_fresh_cohort'] is True
    assert accepted.get('content_sha256'),'Final acceptance must bind its complete input and code manifest'
    for filename,expected in accepted['content_sha256'].items():
        assert digest(Path(filename))==expected, f'Accepted input changed before fresh generation: {filename}'
    seed=accepted['generic_geometry_seed'];assert type(seed) is int
    assert Path(accepted['library']).resolve()==args.library.resolve()
    assert accepted['library_sha256']==digest(args.library)
    sources=json.loads(args.prepared.read_text())['sources']
    assert set(DATASETS).issubset({source['id'] for source in sources})
    args.output.mkdir()
    definitions=[]
    for dataset,cases in DATASETS.items():
        for case in cases:
            for regime in ['local','http0']:
                lanes,native,upstream,strategy=recipe(dataset,case,regime)
                destination=args.output/f'{dataset}-{case}-{regime}'
                command=[str(args.python),str(HERE/'programme.py'),'freeze','--output',str(destination),
                    '--prepared',str(args.prepared),'--library',str(args.library),'--python',str(args.python),
                    '--datasets',dataset,'--cases',case,'--lanes',','.join(lanes),'--native-reference',native,
                    f'--ee-{strategy}-reference',upstream,'--regimes',regime,'--repeats','3','--seed',str(seed)]
                for dependency in args.runtime_dependency:command+=['--runtime-dependency',str(dependency)]
                definitions.append({'dataset':dataset,'case':case,'regime':regime,'output':str(destination),'freeze_command':command})
    random.Random(seed+691).shuffle(definitions)
    write(args.output/'definition.json',{'schema':'skarve_final_generic_definition_v1',
        'acceptance_sha256':digest(args.acceptance),'prepared_sha256':digest(args.prepared),
        'driver_sha256':digest(Path(__file__)),'geometry_seed':seed,'jobs':672,'quadrants':14,
        'commands':definitions,'schedule':'Seeded order of28dataset/quadrant/regime blocks;24randomized jobs per block. All geometry and execution definitions freeze before any query timing. Local/HTTP layouts are selected from pre-final development, never fresh outcomes.',
        'repeats':'Three independent processes with predeclared repeat-specific geometries from programme.py; paired lanes share each exact geometry. No p95 claim.',
        'failures':'Keep every execution/admission and numerical-reference failure, including ordinaryTIFF and optional-backend controls. Never turn failed operations into speed observations.'})
    for definition in definitions:
        result=subprocess.run(definition['freeze_command'],text=True,capture_output=True)
        write(args.output/(Path(definition['output']).name+'-freeze-process.json'),
              {'returncode':result.returncode,'stdout':result.stdout,'stderr':result.stderr})
        result.check_returncode()
        frozen=json.loads((Path(definition['output'])/'freeze.json').read_text())
        assert len(frozen['tasks'])==24 and frozen['bindings_mode']=='installed'
        definition['freeze_sha256']=digest(Path(definition['output'])/'freeze.json')
    write(args.output/'frozen.json',{'schema':'skarve_final_generic_frozen_v1','jobs':672,
        'library':str(args.library),'python':str(args.python),'library_sha256':digest(args.library),
        'definition_sha256':digest(args.output/'definition.json'),'programmes':definitions})
    print(json.dumps({'passed':True,'frozen_jobs':672,'programmes':28,'output':str(args.output)}))


def run(args):
    frozen=json.loads((args.output/'frozen.json').read_text())
    assert frozen['library_sha256']==digest(Path(frozen['library']))
    assert frozen['definition_sha256']==digest(args.output/'definition.json')
    receipts=[]
    for item in frozen['programmes']:
        destination=Path(item['output']);assert digest(destination/'freeze.json')==item['freeze_sha256']
        command=[frozen['python'],str(HERE/'programme.py'),'run','--output',str(destination),
                 '--library',frozen['library'],'--python',frozen['python']]
        with (args.output/(destination.name+'-run.log')).open('x') as log:
            result=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,text=True)
        receipt={'dataset':item['dataset'],'case':item['case'],'regime':item['regime'],'returncode':result.returncode,'command':command}
        path=destination/'programme.json'
        if path.exists():
            data=json.loads(path.read_text());receipt.update({'jobs':len(data['records']),
                'passed':data['passed'],'execution_failures':sum(not r['passed'] for r in data['records']),
                'numerical_failures':sum(not r['passed'] for r in data['format_validation']),
                'programme_sha256':digest(path)})
        else:receipt.update({'jobs':0,'passed':False,'error':'Programme did not produce complete results; preserve process log'})
        receipts.append(receipt);print(json.dumps(receipt),flush=True)
    result={'schema':'skarve_final_generic_complete_v1','planned_jobs':672,
        'completed_jobs':sum(r['jobs'] for r in receipts),'programmes':receipts,
        'all_operations_successful':all(r['passed'] for r in receipts),
        'interpretation':'Completion preserves losses/rejections; all_operations_successful is not a prerequisite for reporting the complete fair comparison.'}
    write(args.output/'complete.json',result)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('stage',choices=['freeze','run']);parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--acceptance',type=Path);parser.add_argument('--prepared',type=Path)
    parser.add_argument('--library',type=Path);parser.add_argument('--python',type=Path,default=Path(sys.executable))
    parser.add_argument('--runtime-dependency',type=Path,action='append',default=[])
    args=parser.parse_args();freeze(args) if args.stage=='freeze' else run(args)


if __name__=='__main__':main()
