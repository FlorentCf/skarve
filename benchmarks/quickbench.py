#!/usr/bin/env python3
"""One-command credential-free benchmark smoke or full source confirmation."""
import argparse
import json
from pathlib import Path
import subprocess
import sys

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--smoke',action='store_true')
p.add_argument('--artifacts',type=Path,help='Optional existing package artifact directory; packages must already be installed')
p.add_argument('--output',required=True,type=Path)
p.add_argument('--library')
p.add_argument('--expected-library-sha256')
p.add_argument('--seed',type=int)
a=p.parse_args()
if a.artifacts:
 assert a.artifacts.is_dir(),'The declared artifact directory does not exist'
 manifest=json.loads((a.artifacts/'manifest.json').read_text())
 expected=manifest['library_sha256']
 if a.expected_library_sha256:assert a.expected_library_sha256==expected,'Explicit library hash disagrees with artifact manifest'
 a.expected_library_sha256=expected
script=Path(__file__).with_name('quick_benchmark.py')
common=['--output',str(a.output)]
if a.library:common+=['--library',a.library]
freeze=[sys.executable,str(script),'freeze',*common]
if a.smoke:freeze+=['--quick']
if a.seed is not None:freeze+=['--seed',str(a.seed)]
if a.expected_library_sha256:freeze+=['--expected-library-sha256',a.expected_library_sha256]
subprocess.run(freeze,check=True)
subprocess.run([sys.executable,str(script),'run',*common],check=True,timeout=180 if a.smoke else 600)
print(json.dumps({'output':str(a.output),'scope':'smoke' if a.smoke else 'full','report_command':f'{sys.executable} benchmarks/report.py --input {a.output}'}))
