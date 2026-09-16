#!/usr/bin/env python3
"""Install immutable archives into a fresh consumer and test generated-only workflows.

Requires a local test wheelhouse for numpy==2.5.3, rasterio==1.5.1 and blake3==1.0.8 with their
dependencies. No package index, private repository, source credential or native
development override is used. Run inside the release isolation envelope when
qualifying absence of private mounts; this script alone is not a sandbox.
The guarded native-stack regression additionally requires cc as a test-only tool.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT=Path(__file__).resolve().parents[1]
EXPECTED_NUMPY='2.5.3'
EXPECTED_RASTERIO='1.5.1'
EXPECTED_BLAKE3='1.0.8'


def sha(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--artifacts',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--wheelhouse',type=Path,required=True);p.add_argument('--scratch',type=Path)
    args=p.parse_args()
    artifacts=args.artifacts.resolve();wheelhouse=args.wheelhouse.resolve()
    assert not args.output.exists(),'Use a new output receipt'
    assert artifacts.is_dir() and wheelhouse.is_dir()
    scratch=args.scratch.resolve() if args.scratch else Path(tempfile.mkdtemp(prefix='skarve-consumer-'))
    if args.scratch:scratch.mkdir(parents=True,exist_ok=False)
    assert not scratch.is_relative_to(ROOT),'Consumer must be outside the source checkout'
    env={key:os.environ[key] for key in ('PATH','LANG','LC_ALL','TZ') if key in os.environ}
    env.update(HOME=str(scratch/'home'),TMPDIR=str(scratch/'tmp'),OPENBLAS_NUM_THREADS='1',
               OMP_NUM_THREADS='1',GDAL_NUM_THREADS='1',GDAL_CACHEMAX='16',PYTHONNOUSERSITE='1',
               PIP_DISABLE_PIP_VERSION_CHECK='1',npm_config_update_notifier='false')
    (scratch/'home').mkdir();(scratch/'tmp').mkdir()
    shutil.copytree(ROOT/'examples',scratch/'examples');shutil.copytree(ROOT/'tests',scratch/'tests')
    receipt={'schema':1,'passed':False,'scope':'Fresh installed generated-only consumer; no external network or private application reference.',
             'runtime_scope':'Same execution host unless an enclosing isolation receipt states otherwise; not independent hardware replication.',
             'artifact_hashes':{},'checks':[],'limits':{'child_address_space_bytes':2*1024**3,'child_timeout_seconds':120},
             'test_script_sha256':sha(__file__),'new_external_network_requests':0}
    def bounds():resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3))
    def run(name,command,expected=0,parse=False):
        result=subprocess.run([str(v) for v in command],cwd=scratch,env=env,stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=120,preexec_fn=bounds)
        note={'name':name,'exit_code':result.returncode,'expected_exit_code':expected,
              'stdout_sha256':hashlib.sha256(result.stdout.encode()).hexdigest(),
              'stderr_sha256':hashlib.sha256(result.stderr.encode()).hexdigest()}
        receipt['checks'].append(note)
        if result.returncode!=expected:
            # Error text stays in the local console; the portable receipt stores hashes.
            raise RuntimeError(name+' failed\n'+result.stderr[-6000:]+'\n'+result.stdout[-6000:])
        def unwrap(value):
            if isinstance(value,dict) and 'ok' in value and 'result' in value:
                assert value['ok'] is True,'CLI returned an unsuccessful result'
                return value['result']
            return value
        if parse=='jsonl':note['result']=[unwrap(json.loads(line)) for line in result.stdout.splitlines() if line.strip()]
        elif parse:note['result']=unwrap(json.loads(result.stdout))
        return result
    try:
        wheels=list(artifacts.glob('*.whl'));npm=list(artifacts.glob('*.tgz'))
        cli=[path for path in artifacts.glob('*.tar.gz') if 'linux' in path.name]
        assert len(wheels)==len(npm)==len(cli)==1,'Need exactly one matching wheel/npm/CLI archive'
        for path in (wheels[0],npm[0],cli[0]):receipt['artifact_hashes'][path.name]={'sha256':sha(path),'bytes':path.stat().st_size}
        manifest_path=artifacts/'manifest.json';manifest=None
        if manifest_path.exists():
            manifest=json.loads(manifest_path.read_text());receipt['manifest_sha256']=sha(manifest_path)
            for entry in manifest.get('artifacts',[]):
                if entry['name'] in receipt['artifact_hashes']:
                    assert receipt['artifact_hashes'][entry['name']]['sha256']==entry['sha256']
        run('create-venv',[sys.executable,'-m','venv',scratch/'venv'])
        python=scratch/'venv/bin/python'
        run('install-wheel-offline',[python,'-m','pip','install','--no-index','--no-deps',wheels[0]])
        run('install-test-only-dependencies-offline',[python,'-m','pip','install','--no-index','--find-links',wheelhouse,
             'numpy=='+EXPECTED_NUMPY,'rasterio=='+EXPECTED_RASTERIO,'blake3=='+EXPECTED_BLAKE3])
        run('install-node-offline',['npm','install','--offline','--ignore-scripts','--omit=optional','--no-audit','--no-fund',npm[0]])
        with tarfile.open(cli[0]) as archive:
            members=archive.getmembers();assert len(members)<=2048
            assert sum(item.size for item in members)<=128*1024**2
            for item in members:
                assert not item.issym() and not item.islnk() and (item.isfile() or item.isdir())
                assert (scratch/item.name).resolve().is_relative_to(scratch)
            archive.extractall(scratch,filter='data')
        binaries=list(scratch.glob('skarve-*/bin/skarve'));assert len(binaries)==1
        command=binaries[0]
        run('cli-version',[command,'--version'])
        run('wheel-cli-version',[scratch/'venv/bin/skarve','--version'])
        run('cli-backend-discovery',[command,'backends'],parse=True)
        backends=receipt['checks'][-1]['result']
        assert backends['native'] is True and isinstance(backends['exactextract'],bool)
        assert backends['default_backend']=='native' and backends['default_auto_policy']=='native_only'
        receipt['backends']=backends
        if manifest is not None:
            assert manifest['backends']==backends,'Artifact manifest and installed CLI backend availability differ'
        run('generate-original-and-cog',[python,scratch/'examples/generate_fixtures.py',scratch/'data'],parse=True)
        fixture=scratch/'data/fixture.json'
        generated=json.loads(fixture.read_text());receipt['generated_fixture_sha256']=sha(fixture)
        receipt['fixture_dependencies']=generated['dependencies']
        source=scratch/'data'/generated['sources']['original']['file'];polygon=scratch/'data/polygon.json'
        # Resolve both bindings and their native libraries from fresh installations.
        # Rasterio supplies an independent raw-sample/mask oracle for a small window.
        window_python=scratch/'installed_source_window.py'
        window_python.write_text("""import hashlib, json, pathlib, sys
import rasterio
from rasterio.windows import Window
import skarve
from skarve import Skarve
assert pathlib.Path(skarve.__file__).resolve().is_relative_to(pathlib.Path(sys.prefix).resolve())
def digest(value): return hashlib.sha256(value).hexdigest()
window=[0,0,8,8]
bands=[2,0]
with rasterio.open(sys.argv[1]) as dataset:
    samples=dataset.read([i+1 for i in bands], window=Window(*window))
    masks=dataset.read_masks([i+1 for i in bands], window=Window(*window))
    expected=[dict(sourceBand=band,
                   samples=digest(samples[i].astype(samples.dtype.newbyteorder('<'),copy=False).tobytes()),
                   mask=digest(masks[i].tobytes())) for i,band in enumerate(bands)]
with Skarve() as engine:
    with engine.infuse(sys.argv[1]) as source:
        result=source.read_window(window,bands)
# Keep and inspect views after their source and engine have closed.
assert result['abiVersion']==1 and result['width']==8 and result['height']==8
assert len(result['bands'])==len(expected)
for actual, oracle in zip(result['bands'],expected):
    assert actual['sourceBand']==oracle['sourceBand']
    assert digest(actual['values'].tobytes())==oracle['samples']
    assert digest(actual['mask'].tobytes())==oracle['mask']
pathlib.Path(sys.argv[2]).write_text(json.dumps(dict(window=window,bands=bands,expected=expected)))
print(json.dumps(dict(passed=True,bands=len(expected),cells_per_band=64,retained_after_close=True)))
""")
        window_oracle=scratch/'source-window-oracle.json'
        run('python-installed-original-window',[python,window_python,source,window_oracle],parse=True)
        window_node=scratch/'installed_source_window.mjs'
        window_node.write_text("""import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {resolve, sep} from 'node:path';
import Skarve from '@skarve/engine';
assert(fileURLToPath(import.meta.resolve('@skarve/engine')).startsWith(resolve('node_modules')+sep));
const oracle=JSON.parse(readFileSync(process.argv[3],'utf8'));
const digest=view=>createHash('sha256').update(Buffer.from(view.buffer,view.byteOffset,view.byteLength)).digest('hex');
const engine=new Skarve();
let result;
try {
  const source=await engine.infuse(process.argv[2]);
  try {result=await source.readWindow({window:oracle.window,bands:oracle.bands});}
  finally {await source.close();}
} finally {await engine.close();}
assert.equal(result.abiVersion,1);
assert.equal(result.width,8); assert.equal(result.height,8);
assert.equal(result.bands.length,oracle.expected.length);
for (let i=0;i<oracle.expected.length;i++) {
  const actual=result.bands[i], expected=oracle.expected[i];
  assert.equal(actual.sourceBand,expected.sourceBand);
  assert.equal(digest(actual.values),expected.samples);
  assert.equal(digest(actual.mask),expected.mask);
}
console.log(JSON.stringify({passed:true,bands:result.bands.length,cells_per_band:64,retained_after_close:true}));
""")
        run('node-installed-original-window',['node','--max-old-space-size=512',window_node,source,window_oracle],parse=True)
        run('cli-inspect',[command,'inspect',source],parse=True)
        run('cli-infuse-alias',[command,'infuse',source],parse=True)
        run('cli-measure',[command,'measure',source,polygon,'--crs','EPSG:3857','--bands','0','--statistics','sum,support,mean,min,max,count'],parse=True)
        native_bands=receipt['checks'][-1]['result']['bands']
        run('cli-carve-alias',[command,'carve',source,polygon,'--crs','EPSG:3857','--bands','0','--metrics','sum,support,mean,min,max,count','--backend','native'],parse=True)
        assert receipt['checks'][-1]['result']['bands']==native_bands
        run('cli-requires-explicit-crs',[command,'measure',source,polygon],expected=1)
        run('cli-prepare',[command,'prepare',source,scratch/'cli-index','--boundary-source','original'],parse=True)
        run('cli-ward-alias',[command,'ward',source,scratch/'cli-ward-index','--boundary-source','original'],parse=True)
        run('cli-measure-original-index',[command,'measure',source,polygon,'--crs','EPSG:3857','--index',scratch/'cli-index'],parse=True)
        statistics=['sum','support','mean','min','max','count']
        job={'zones':[{'id':name,'version':'1','geometry':generated['geometries'][name]} for name in ('small','overlap','other')],
             'slices':[{'id':f'{name}-band{band}','spec':{'location':str(scratch/'data'/generated['sources'][name]['file'])},'bands':[band]}
                       for name in ('original','date-b') for band in (2,0,1)],
             'crs':generated['crs'],'tile_edge':32,
             'budget':{'working_bytes':96*1024**2,'geometry_bytes':8*1024**2,'tile_bytes':8*1024**2,
                       'output_bytes':1024**2,'max_windows':64,'decoded_bytes':8*1024**2,
                       'max_contributions':1024**2,'workers':1},'options':{'statistics':statistics}}
        job_path=scratch/'batch.json';job_path.write_text(json.dumps(job))
        run('cli-shared-scan-batch',[command,'batch',job_path,'--max-rows','2'],parse='jsonl')
        pages=receipt['checks'][-1]['result']
        assert pages[-1]['complete'] and sum(len(page['rows']) for page in pages)==18
        assert all(len(page['rows'])<=2 for page in pages)
        receipt['checks'][-1]['result']={'rows':18,'maximum_page_rows':max(len(page['rows']) for page in pages),
                                       'complete':True,'metrics':pages[-1]['metrics']}
        branded_job={**job,'metrics':statistics};branded_job.pop('options')
        branded_job_path=scratch/'cleave.json';branded_job_path.write_text(json.dumps(branded_job))
        run('cli-cleave-alias',[command,'cleave',branded_job_path,'--max-rows','2'],parse='jsonl')
        branded_pages=receipt['checks'][-1]['result']
        assert branded_pages[-1]['complete'] and sum(len(page['rows']) for page in branded_pages)==18
        assert all(len(page['rows'])<=2 for page in branded_pages)
        assert [row['bands'] for page in branded_pages for row in page['rows']]==[row['bands'] for page in pages for row in page['rows']]
        receipt['checks'][-1]['result']={'rows':18,'maximum_page_rows':max(len(page['rows']) for page in branded_pages),
                                       'complete':True,'metrics':branded_pages[-1]['metrics']}
        session_path=scratch/'session.jsonl'
        session_commands=[{'op':'open','id':'generated','spec':{'location':str(source)}},
                          {'op':'measure','source':'generated','geometry':generated['geometries']['small'],
                           'crs':generated['crs'],'bands':[0],'statistics':statistics},
                          {'op':'close','source':'generated'}]
        session_path.write_text(''.join(json.dumps(value)+'\n' for value in session_commands))
        run('cli-source-session',[command,'session',session_path],parse='jsonl')
        assert len(receipt['checks'][-1]['result'])==3
        branded_session=scratch/'branded-session.jsonl'
        branded_commands=[{'op':'infuse','id':'generated','spec':{'location':str(source)}},
                          {'op':'carve','source':'generated','zone':generated['geometries']['small'],
                           'crs':generated['crs'],'bands':[0],'metrics':statistics,'backend':'native'},
                          {'op':'close','source':'generated'}]
        branded_session.write_text(''.join(json.dumps(value)+'\n' for value in branded_commands))
        run('cli-branded-source-session',[command,'session',branded_session],parse='jsonl')
        assert len(receipt['checks'][-1]['result'])==3
        assert receipt['checks'][-1]['result'][1]['bands']==native_bands
        run('python-source-example',[python,scratch/'examples/python_source.py','--fixture',fixture,'--index',scratch/'python-example-index'],parse=True)
        run('python-typed-example',[python,scratch/'examples/python_bulk.py'],parse=True)
        run('node-source-example',['node',scratch/'examples/node_source.mjs',fixture,scratch/'node-example-index'],parse=True)
        run('node-typed-example',['node',scratch/'examples/node_bulk.mjs'],parse=True)
        # Experimental SKV v0 is qualified through the ordinary installed
        # packages. These generated files are independent of any private data;
        # their format version is explicitly not a frozen compatibility promise.
        run('python-experimental-skv-example',[python,scratch/'examples/python_skv.py',
            '--fixture',fixture,'--output',scratch/'python-example.skv'],parse=True)
        run('node-experimental-skv-example',['node',scratch/'examples/node_skv.mjs',
            fixture,scratch/'node-example.skv'],parse=True)
        run('python-experimental-skv-predictor-example',[python,scratch/'examples/python_skv.py',
            '--fixture',fixture,'--output',scratch/'python-predictor-example.skv',
            '--predictor','byte_delta_v1'],parse=True)
        run('node-experimental-skv-predictor-example',['node',scratch/'examples/node_skv.mjs',
            fixture,scratch/'node-predictor-example.skv','byte_delta_v1'],parse=True)
        run('python-experimental-skv-group-example',[python,scratch/'examples/python_skv.py',
            '--fixture',fixture,'--output',scratch/'python-group-example.skv',
            '--predictor','byte_delta_v1','--payload-layout','row_group_v1'],parse=True)
        run('node-experimental-skv-group-example',['node',scratch/'examples/node_skv.mjs',
            fixture,scratch/'node-group-example.skv','byte_delta_v1','row_group_v1'],parse=True)
        run('native-experimental-skv-guarded-stack',[python,scratch/'tests/skv_stack_guard.py',
            '--library',command.parent.parent/'lib/libraster_engine.so','--source',source,
            '--scratch',scratch/'skv-stack-guard'],parse=True)
        run('native-experimental-skv-predictor-guarded-stack',[python,scratch/'tests/skv_stack_guard.py',
            '--library',command.parent.parent/'lib/libraster_engine.so','--source',source,
            '--scratch',scratch/'skv-predictor-stack-guard','--predictor','byte_delta_v1'],parse=True)
        overview_fixture=scratch/'generated-overview.tif'
        run('generate-internal-overview-guard-fixture',[python,'-c',
            "import shutil, sys\nimport rasterio\nfrom rasterio.enums import Resampling\n"
            "shutil.copy2(sys.argv[1], sys.argv[2])\n"
            "with rasterio.open(sys.argv[2], 'r+') as dataset:\n"
            "    dataset.build_overviews([2], Resampling.nearest)\n",source,overview_fixture])
        run('native-experimental-skv-overview-guarded-stack',[python,scratch/'tests/skv_stack_guard.py',
            '--library',command.parent.parent/'lib/libraster_engine.so','--source',overview_fixture,
            '--scratch',scratch/'skv-overview-stack-guard','--overview','0',
            '--predictor','byte_delta_v1','--ordered'],parse=True)
        overview_guard=receipt['checks'][-1]['result']
        assert overview_guard['source_overview']==0
        assert overview_guard['ordered_operation']
        assert all(check['compiled_grid']['width']==32 and check['compiled_grid']['height']==32
                   for check in overview_guard['checks'])
        run('python-cli-experimental-skv-four-cases',[python,scratch/'tests/skv_installed.py',
            '--scratch',scratch/'skv-contracts','--binary',command],parse=True)
        skv_python=receipt['checks'][-1]['result']
        assert skv_python['passed'] and skv_python['original_path_unavailable']
        assert skv_python['exactextract']==backends['exactextract']
        assert set(skv_python['predictor_receipts'])=={'none','deflate'}
        assert set(skv_python['group_receipts'])=={'none','deflate'}
        run('node-experimental-skv-four-cases',['node','--max-old-space-size=512',
            scratch/'tests/skv_installed_node.mjs',scratch/'skv-contracts/fixture.json'],parse=True)
        skv_node=receipt['checks'][-1]['result']
        assert skv_node['passed'] and skv_node['exactextract']==backends['exactextract']
        assert {item['codec'] for item in skv_node['predictorReceipts']}=={'none','deflate'}
        assert {item['codec'] for item in skv_node['groupReceipts']}=={'none','deflate'}
        run('native-experimental-skv-grouped-forty-guarded-stack',[python,scratch/'tests/skv_stack_guard.py',
            '--library',command.parent.parent/'lib/libraster_engine.so',
            '--source',scratch/'skv-contracts/forty.unavailable','--scratch',scratch/'skv-grouped-forty-stack-guard',
            '--predictor','byte_delta_v1','--payload-layout','row_group_v1','--ordered'],parse=True)
        assert all(check['ordered_bands']==40 for check in receipt['checks'][-1]['result']['checks'])
        receipt['experimental_skv_v0']={'passed':True,'format_stability':'experimental',
            'bands':40,'cases':['1x1','Nx1','1xM','NxM'],'original_source_required_for_serving':False,
            'predictors':['none','byte_delta_v1'],'codecs':['none','deflate'],
            'payload_layouts':['band','row_group_v1'],
            'summaries_enabled_and_disabled':True,'exactextract_qualified':backends['exactextract']}
        run('python-serving-profile-contracts',[python,scratch/'tests/serving_profile_installed.py',
            '--fixture',scratch/'skv-contracts/fixture.json','--output',scratch/'serving-profile'],parse=True)
        serving_profile=receipt['checks'][-1]['result']
        assert serving_profile['passed'] and serving_profile['independent_complete_equivalence']
        run('node-serving-profile-contracts',['node',scratch/'tests/serving_profile_node.mjs',
            scratch/'serving-profile'],parse=True)
        assert receipt['checks'][-1]['result']['rows']==serving_profile['rows']
        run('python-serving-profile-example',[python,scratch/'examples/python_serving.py',
            '--profile',serving_profile['profile'],'--request',serving_profile['request'],
            '--view-id',serving_profile['view_id'],'--access-class',serving_profile['access_class']],parse=True)
        assert receipt['checks'][-1]['result']['routing']['selected']=='accelerated'
        run('node-serving-profile-example',['node',scratch/'examples/node_serving.mjs',
            serving_profile['profile'],serving_profile['request'],serving_profile['view_id']],parse=True)
        assert receipt['checks'][-1]['result']['routing']['selected']=='direct'
        run('cli-serving-profile-example',[command,'sum-selected','--profile',serving_profile['profile'],
            serving_profile['request'],'--view-id',serving_profile['view_id'],
            '--access-class',serving_profile['access_class'],
            '--numerical-policy','hm_demographics_ordered_v1'],parse=True)
        assert receipt['checks'][-1]['result']['rows']==serving_profile['rows']
        assert receipt['checks'][-1]['result']['routing']['selected']=='accelerated'
        receipt['serving_profile']={'passed':True,'generated_only':True,
            'independent_full_view_comparison':True,'ordinary_cli_python_node':True,
            'unknown_access_and_sparse_selection':'direct','qualified_fixture_selection':'accelerated',
            'performance_recommendation':False}
        run('python-ordered-source-example',[python,scratch/'examples/python_ordered.py',
            '--fixture',fixture,'--scratch',scratch/'python-ordered'],parse=True)
        ordered_python=receipt['checks'][-1]['result']
        assert ordered_python['passed'] and ordered_python['source_original_unavailable']
        assert ordered_python['policy']=='hm_demographics_ordered_v1'
        run('node-ordered-source-example',['node',scratch/'examples/node_ordered.mjs',
            fixture,scratch/'node-ordered'],parse=True)
        ordered_node=receipt['checks'][-1]['result']
        assert ordered_node['passed'] and ordered_node['source_original_unavailable']
        assert ordered_node['rows']==ordered_python['rows']
        run('cli-ordered-source-example',[command,'sum-selected',ordered_python['source'],
            ordered_python['selection'],'--numerical-policy','hm_demographics_ordered_v1'],parse=True)
        ordered_cli=receipt['checks'][-1]['result']
        assert ordered_cli['complete'] and ordered_cli['rows']==ordered_python['rows']
        assert ordered_cli['provenance']['summaries_used'] is False
        run('cli-ordered-requires-explicit-policy',[command,'sum-selected',ordered_python['source'],
            ordered_python['selection']],expected=1)
        receipt['ordered_source']={'passed':True,'policy':'hm_demographics_ordered_v1',
            'generated_only':True,'source_mapping':[2,0,1],'selected_bands':[1,0],
            'selection':'compact runs and indexes in shifted overlapping logical windows',
            'rows':ordered_python['rows'],'fractional_normalization_is_separate':True,
            'original_source_required_for_skv_serving':False,
            'rounding_scope':ordered_python['fixture_rounding_scope']}
        run('python-generated-contracts',[python,scratch/'tests/python_contracts.py','--fixture',fixture,'--scratch',scratch/'python-contracts'],parse=True)
        run('node-generated-contracts',['node','--max-old-space-size=512',scratch/'tests/node_contracts.mjs',fixture,scratch/'node-contracts'],parse=True)
        common=['sum','support','mean','min','max']
        ee_commands=[('python',[python,scratch/'examples/python_backend.py','--fixture',fixture,'--backend','exactextract']),
                     ('node',['node',scratch/'examples/node_backend.mjs',fixture,'exactextract']),
                     ('cli',[command,'carve',source,polygon,'--crs',generated['crs'],'--bands','0,2',
                             '--metrics',','.join(common),'--backend','exactextract'])]
        if backends['exactextract']:
            complete=[]
            for interface,arguments in ee_commands:
                run(interface+'-exactextract-source-and-batch' if interface!='cli' else 'cli-exactextract-source',arguments,parse=True)
                value=receipt['checks'][-1]['result']
                single=value if interface=='cli' else value['single']
                provenance=single['provenance']
                assert provenance['selected_backend']=='exactextract'
                assert provenance['numerical_policy']=='exactextract_fractional_v030'
                assert provenance['source_interpretation']=='skarve_normalized_f64_v1'
                assert provenance['upstream_version']=='0.3.0'
                assert all('valid_cell_count' not in band for band in single['bands'])
                if interface!='cli':
                    assert value['batch_rows']==8
                    assert value['batch_provenance']['selected_backend']=='exactextract'
                complete.append(single['bands'])
            assert complete[0]==complete[1]==complete[2],'Installed callers disagree on the same delegated result'
            ee_job={'zones':[{'id':name,'version':'1','geometry':generated['geometries'][name]} for name in ('small','overlap','thin','outside')],
                    'slices':[{'id':name,'spec':{'location':str(scratch/'data'/generated['sources'][name]['file'])},'bands':[0,2]}
                              for name in ('original','date-b')],
                    'crs':generated['crs'],'metrics':common,'backend':'exactextract',
                    'budget':job['budget']}
            ee_job_path=scratch/'exactextract-cleave.json';ee_job_path.write_text(json.dumps(ee_job))
            rejected=run('cli-exactextract-rejects-insufficient-reader-admission',
                         [command,'cleave',ee_job_path,'--max-rows','2'],expected=1)
            assert 'all-reader admission exceeds working_bytes before source open' in rejected.stderr
            # Exactextract stages a complete finite result and conservatively
            # admits all readers plus its fixed upstream reserve before opening.
            # The 96 MiB native streaming fixture budget is intentionally too low.
            ee_job['budget']={**ee_job['budget'],'working_bytes':256*1024**2}
            ee_job_path.write_text(json.dumps(ee_job))
            run('cli-exactextract-bounded-cleave',[command,'cleave',ee_job_path,'--max-rows','2'],parse='jsonl')
            ee_pages=receipt['checks'][-1]['result']
            assert ee_pages[-1]['complete'] and sum(len(page['rows']) for page in ee_pages)==8
            assert all(len(page['rows'])<=2 and page['checkpoint'] is None and not page['checkpoint_supported'] for page in ee_pages)
            assert all(page['provenance']['selected_backend']=='exactextract' for page in ee_pages)
            assert len({row['result_id'] for page in ee_pages for row in page['rows']})==8
            receipt['checks'][-1]['result']={'rows':8,'complete':True,'checkpoint_supported':False,
                'maximum_page_rows':max(len(page['rows']) for page in ee_pages),
                'provenance':ee_pages[-1]['provenance'],'metrics':ee_pages[-1]['metrics']}
            run('cli-auto-sole-accepted-exactextract',[command,'carve',source,polygon,'--crs',generated['crs'],
                '--metrics',','.join(common),'--backend','auto','--accepted-policies','exactextract_fractional_v030'],parse=True)
            assert receipt['checks'][-1]['result']['provenance']['selected_backend']=='exactextract'
        else:
            for interface,arguments in ee_commands:
                unavailable=run(interface+'-explicit-exactextract-unavailable',arguments,expected=1)
                assert 'exactextract' in unavailable.stderr and 'not installed' in unavailable.stderr
        run('cli-auto-strict-default',[command,'carve',source,polygon,'--crs',generated['crs'],'--bands','0',
            '--metrics',','.join(common),'--backend','auto'],parse=True)
        assert receipt['checks'][-1]['result']['provenance']['selected_backend']=='native'
        run('generate-selected-metric-fixture',[python,scratch/'tests/product_api_metrics.py','generate',scratch/'selected-metrics'],parse=True)
        selected_fixture=scratch/'selected-metrics/fixture.json'
        for backend in (['native','exactextract'] if backends['exactextract'] else ['native']):
            run('python-'+backend+'-selected-metrics',[python,scratch/'tests/product_api_metrics.py','check',selected_fixture,'--backend',backend],parse=True)
            run('node-'+backend+'-selected-metrics',['node',scratch/'tests/product_api_metrics.mjs',selected_fixture,backend],parse=True)
            run('cli-'+backend+'-min-without-overflowing-sum',[command,'carve',scratch/'selected-metrics/large-finite.tif',
                scratch/'selected-metrics/zone.json','--crs','EPSG:3857','--metrics','min','--backend',backend],parse=True)
            band=receipt['checks'][-1]['result']['bands'][0]
            assert band['min']==1e308 and 'fractional_sum' not in band
        receipt['passed']=True
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True)
        # Replace generated absolute paths recursively; retain all numeric/boolean leaves.
        def clean(value):
            if isinstance(value,dict):return {key:clean(item) for key,item in value.items()}
            if isinstance(value,list):return [clean(item) for item in value]
            if isinstance(value,str):
                for path,token in ((scratch,'<consumer>'),(artifacts,'<artifacts>'),(wheelhouse,'<wheelhouse>'),(ROOT,'<source>')):
                    value=value.replace(str(path),token)
            return value
        with args.output.open('x') as output:json.dump(clean(receipt),output,indent=2,allow_nan=False);output.write('\n')
    print(json.dumps({'passed':receipt['passed'],'checks':len(receipt['checks']),'new_external_network_requests':0}))


if __name__=='__main__':main()
