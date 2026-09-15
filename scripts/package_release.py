#!/usr/bin/env python3
"""Assemble versioned Linux artifacts and complete source from a clean Skarve checkout."""
import argparse, gzip, hashlib, io, json, os, platform, re, shutil, subprocess, sys, tarfile, tempfile, tomllib
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
RUNTIME = "Ubuntu 24.04 x86-64; libgdal34t64 and gdal-data 3.8.4+dfsg-3ubuntu3; libdeflate0 1.19-1build1.1; system glibc. Linux SKV decoding directly requires libdeflate.so.0 and its per-context allocation API. No Windows/macOS execution claimed."
RECIPE = "sudo apt-get update && sudo apt-get install libgdal34t64=3.8.4+dfsg-3ubuntu3 gdal-data=3.8.4+dfsg-3ubuntu3 libdeflate0=1.19-1build1.1"

def sha(path):
    with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def run(command, **kwargs):
    result=subprocess.run([str(x) for x in command],text=True,capture_output=True,**kwargs)
    if result.returncode:raise RuntimeError(f"Command failed: {command[0]}\n{result.stderr or result.stdout}")
    return result.stdout

def validate_versions(cli_output):
    match=re.fullmatch(r'(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)\.(\d+))?',VERSION)
    if not match:raise SystemExit('Unsupported release version; use a stable, alpha, beta or rc version.')
    expected_python=match[1]+({'alpha':'a','beta':'b','rc':'rc'}[match[2]]+match[3] if match[2] else '')
    python=tomllib.loads((ROOT/'bindings/python/pyproject.toml').read_text())['project']
    node=json.loads((ROOT/'bindings/node/package.json').read_text())
    native=re.match(r'^skarve\s+(\S+)(?:\s|$)',cli_output.strip())
    if not native or native[1]!=VERSION:raise SystemExit('CLI version does not match the release package version.')
    if python['name']!='skarve-engine' or python['version']!=expected_python:
        raise SystemExit('Python distribution name/version does not match Cargo.')
    if node['name']!='@skarve/engine' or node['version']!=VERSION:
        raise SystemExit('Node distribution name/version does not match Cargo.')
    initializer=(ROOT/'bindings/python/skarve/__init__.py').read_text()
    declared=re.search(r'^__version__\s*=\s*[\'"]([^\'"]+)[\'"]\s*$',initializer,re.MULTILINE)
    if not declared or declared[1]!=expected_python:raise SystemExit('Python import version does not match its distribution.')

def source_snapshot():
    checkout=(ROOT/'.git').exists()
    if checkout:
        if run(['git','status','--porcelain','--untracked-files=normal'],cwd=ROOT).strip():
            raise SystemExit('Commit the complete release source before packaging; output/build directories must be ignored.')
        head=run(['git','rev-parse','HEAD'],cwd=ROOT).strip()
        names=[name for name in run(['git','ls-files','-z'],cwd=ROOT).split('\0') if name]
        expected=None
    else:
        record=ROOT/'SOURCE_MANIFEST.json'
        if record.is_symlink() or not record.is_file():
            raise SystemExit('Use a Git checkout or the complete source distribution with SOURCE_MANIFEST.json.')
        source_record=json.loads(record.read_text());head=source_record['source_commit']
        expected=source_record['source_files_sha256']
        if not isinstance(expected,dict) or not expected:raise SystemExit('Empty source manifest')
        names=list(expected)
    if not re.fullmatch(r'[0-9a-f]{40,64}',head):raise SystemExit('Invalid source commit identity')
    relevant=[];source_hashes={}
    for name in names:
        # This generated record is regenerated once, even after archive users
        # initialize a Git checkout and commit its previous copy.
        if name=='SOURCE_MANIFEST.json':continue
        if not isinstance(name,str) or not name or '\\' in name or ':' in name:
            raise SystemExit('Unsafe source manifest path')
        rel=Path(name)
        if rel.is_absolute() or '..' in rel.parts or rel.as_posix()!=name:
            raise SystemExit('Unsafe source manifest path')
        file=ROOT/rel
        if file.is_symlink() or not file.is_file() or not file.resolve().is_relative_to(ROOT.resolve()):
            raise SystemExit('Source must contain regular files within the source root.')
        digest=sha(file)
        if expected is not None and digest!=expected[name]:
            raise SystemExit('Source archive changed; initialize and commit a Git checkout before packaging modifications.')
        relevant.append(file);source_hashes[name]=digest
    for name in ['Cargo.toml','Cargo.lock','scripts/package_release.py','LICENSE','NOTICE',
                 'bindings/python/pyproject.toml','bindings/node/package.json']:
        if name not in source_hashes:raise SystemExit('Incomplete source manifest: '+name)
    return checkout,head,relevant,source_hashes

def copy_verified(relative,destination,source_hashes):
    relative=Path(relative);name=relative.as_posix();source=ROOT/relative
    if name not in source_hashes or source.is_symlink() or not source.resolve().is_relative_to(ROOT.resolve()):
        raise SystemExit('Unverified package input: '+name)
    destination=Path(destination);destination.parent.mkdir(parents=True,exist_ok=True)
    shutil.copy2(source,destination)
    if sha(destination)!=source_hashes[name]:raise SystemExit('Package input changed while copying: '+name)

def copy_verified_tree(relative,destination,source_hashes):
    relative=Path(relative);source=ROOT/relative;destination=Path(destination)
    # Generated bytecode is ignored; every other copied file must belong to the
    # verified snapshot. Never let copytree introduce ignored/unlisted extras.
    for file in source.rglob('*'):
        rel=file.relative_to(source)
        if '__pycache__' in rel.parts:continue
        if file.is_symlink() or (file.is_file() and file.relative_to(ROOT).as_posix() not in source_hashes):
            raise SystemExit('Unexpected file in package input tree: '+file.relative_to(ROOT).as_posix())
    destination.mkdir(parents=True,exist_ok=True)
    prefix=relative.as_posix()+'/'
    for name in sorted(source_hashes):
        if name.startswith(prefix):copy_verified(name,destination/Path(name).relative_to(relative),source_hashes)

def notice_inventory(destination,binary,exactextract=False):
    destination.mkdir()
    metadata=json.loads(run(['cargo','metadata','--locked','--offline','--format-version=1','--filter-platform=x86_64-unknown-linux-gnu'],cwd=ROOT))
    reviewed=json.loads((ROOT/'third_party/DEPENDENCIES.json').read_text())
    approved={(p['ecosystem'],p['name'],p['version']):p for p in reviewed['packages']}
    def retain(ecosystem,name,version,license_expression):
        record=approved.get((ecosystem,name,version))
        if not record or record['license_expression']!=license_expression:
            raise RuntimeError(f'Unreviewed dependency version or license: {ecosystem} {name} {version}; regenerate the reviewed third_party inventory before packaging.')
        notices=[]
        for entry in record['notices']:
            relative=Path(entry['path'])
            if relative.is_absolute() or '..' in relative.parts or relative.parts[:2]!=('third_party','notices'):
                raise RuntimeError('Invalid reviewed notice path')
            source=ROOT/relative
            if source.is_symlink() or not source.is_file() or sha(source)!=entry['sha256']:
                raise RuntimeError(f'Reviewed dependency notice missing or changed: {relative}')
            target=destination/Path(*relative.parts[2:]);target.parent.mkdir(parents=True,exist_ok=True)
            shutil.copy2(source,target)
            notices.append({'file':target.relative_to(destination).as_posix(),'sha256':entry['sha256']})
        if not notices:raise RuntimeError(f'Dependency has no retained notice: {name}')
        return notices
    records=[]
    for package in metadata['packages']:
        if package['id']==metadata['resolve']['root']:continue
        notices=retain('Rust',package['name'],package['version'],package.get('license'))
        records.append({'ecosystem':'Rust','name':package['name'],'version':package['version'],'license_expression':package.get('license'),'notices':notices,'scope':'Cargo target-resolution inventory includes test/build dependencies; not a legal license conclusion'})
    for name in ['koffi','@koromix/koffi-linux-x64']:
        folder=ROOT/'bindings/node/node_modules'/name;package=json.loads((folder/'package.json').read_text())
        notices=retain('Node',name,package['version'],package.get('license'))
        records.append({'ecosystem':'Node','name':name,'version':package['version'],'license_expression':package.get('license'),'notices':notices})
    if exactextract:
        pin=json.loads((ROOT/'native/exactextract/upstream.json').read_text())
        notices=retain('C++',pin['project'],pin['version'],pin['license'])
        records.append({'ecosystem':'C++','name':pin['project'],'version':pin['version'],
                        'commit':pin['commit'],'archive_sha256':pin['archive_sha256'],
                        'license_expression':pin['license'],'notices':notices,
                        'relationship':'Optional unmodified upstream core statically linked; Skarve bridge separately authored; system GEOS dynamically linked.'})
    for name in ['libgdal34t64','gdal-data']:
        copied=[]
        for label in ['copyright','NOTICE']:
            path=Path('/usr/share/doc')/name/label
            if path.is_file():
                target='external-'+name+'-'+label+'.txt';shutil.copy2(path,destination/target);copied.append({'file':target,'sha256':sha(path)})
        records.append({'ecosystem':'External Ubuntu runtime','name':name,'version':run(['dpkg-query','-W','-f=${Version}',name]).strip(),'bundled':False,'notices':copied})
    dependencies=[]
    for line in run(['ldd',binary]).splitlines():
        if 'not found' in line:raise RuntimeError('Build host has a missing native dependency: '+line.strip())
        match=re.search(r'^\s*(\S+) => (/\S+)',line)
        if match:
            soname,path=match.groups();dependencies.append({'soname':soname,'system_path':path,'sha256':sha(path),'bundled':False})
    return {'dependencies':records,'native_external_libraries':dependencies,'runtime':RUNTIME,'runtime_recipe':RECIPE,'project_license':'Apache-2.0; third-party components retain the license expressions and notices listed here.'}

def local_system_runtime_inventory(binary, inventory, destination):
    """Capture this host for local tests; never claim distribution qualification."""
    libraries = []
    packages = {}
    for library in inventory['native_external_libraries']:
        path = Path(library['system_path'])
        owners = None
        for candidate in dict.fromkeys((str(path), str(path.resolve()), str(path).replace('/lib/', '/usr/lib/', 1))):
            try:
                owners = [line for line in run(['dpkg-query', '-S', candidate]).strip().splitlines()
                          if ': ' in line and not line.startswith('diversion ')]
                if owners:
                    break
            except RuntimeError:
                pass
        if not owners:
            raise RuntimeError('Cannot identify installed system library package: ' + library['soname'])
        names = sorted({line.rsplit(': ', 1)[0] for line in owners
                        if ': ' in line and not line.startswith('diversion ')})
        if not names:
            raise RuntimeError('Cannot identify installed system library package: ' + library['soname'])
        libraries.append(dict(library, packages=names))
        for name in names:
            if name in packages:
                continue
            version = run(['dpkg-query', '-W', '-f=${Version}', name]).strip()
            notice = Path('/usr/share/doc') / name.split(':')[0] / 'copyright'
            if not notice.is_file():
                raise RuntimeError('Missing installed copyright notice: ' + name)
            relative = Path('system-local') / name.replace(':', '_') / 'copyright'
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(notice, target)
            packages[name] = {'package': name, 'version': version,
                              'copyright_record': relative.as_posix(),
                              'copyright_sha256': sha(target), 'bundled': False}
    return {'schema': 'skarve_local_runtime_inventory_v1',
            'distribution_qualified': False,
            'scope': 'Observed installed runtime and retained notices for local build/testing only. No binary distribution clearance or qualified-host equivalence.',
            'inspected_binary_sha256': sha(binary),
            'libraries': libraries, 'packages': list(packages.values())}

def system_runtime_inventory(binary,inventory):
    """Bind reviewed system notices to this binary's actual runtime closure."""
    reviewed=json.loads((ROOT/'third_party/SYSTEM_RUNTIME.json').read_text())
    by_soname={entry['soname']:entry for entry in reviewed['libraries']}
    actual={entry['soname'] for entry in inventory['native_external_libraries']}
    if actual-set(by_soname):raise RuntimeError('Unreviewed system runtime libraries: '+', '.join(sorted(actual-set(by_soname))))
    for package in reviewed['packages']:
        version=run(['dpkg-query','-W','-f=${Version}',package['package']]).strip()
        if version!=package['version']:raise RuntimeError('System package changed since notice review: '+package['package'])
        notice=ROOT/package['copyright_record']
        if notice.is_symlink() or sha(notice)!=package['copyright_sha256']:
            raise RuntimeError('System package notice changed: '+package['package'])
    reviewed['inspected_binary_sha256']=sha(binary)
    reviewed['skarve_direct_needed']=re.findall(r'\(NEEDED\).*\[([^]]+)\]',run(['readelf','-d',binary]))
    reviewed['libraries']=[by_soname[name] for name in sorted(actual)]
    reviewed['scope']='This artifact library actual transitive ldd closure and direct ELF dependencies, verified against retained distribution package versions/notices. License labels are not resolved legal conclusions.'
    reviewed['package_inventory_scope']='Reviewed system package/notice superset; libraries identifies the actual binary closure.'
    return reviewed

def main():
    os.environ['PATH']=str(Path.home()/'.cargo/bin')+os.pathsep+os.environ.get('PATH','')
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary-dir',type=Path,default=ROOT/'target/release');p.add_argument('--output-dir',type=Path,default=ROOT/'dist'/('v'+VERSION));p.add_argument('--expected-library-sha256');p.add_argument('--expected-cli-sha256');p.add_argument('--label',default='Linux x86-64 beta; publication controlled by repository owner');p.add_argument('--local-use-only', action='store_true', help='Capture this host runtime for local installation/CI, without binary distribution qualification. Strict reviewed-runtime checks remain the default.');args=p.parse_args()
    if platform.system()!='Linux' or platform.machine()!='x86_64':raise SystemExit('Only the tested Linux x86-64 package assembly is supported.')
    try:run([sys.executable,'-m','build','--version'])
    except Exception as error:raise SystemExit('Build tools missing. Create a build venv and install scripts/packaging-requirements.txt. '+str(error))
    library=args.binary_dir/'libraster_engine.so';binary=args.binary_dir/'raster-engine'
    for path in [library,binary]:
        if not path.is_file():raise SystemExit(f'Missing {path.name}; run python scripts/build.py first.')
    library_sha,binary_sha=sha(library),sha(binary)
    if args.expected_library_sha256 and library_sha!=args.expected_library_sha256:raise SystemExit('Native library differs from the requested frozen identity.')
    if args.expected_cli_sha256 and binary_sha!=args.expected_cli_sha256:raise SystemExit('Native CLI differs from the requested frozen identity.')
    checkout,head,relevant,source_hashes=source_snapshot()
    validate_versions(run([binary,'--version']))
    capabilities=json.loads(run([binary,'backends']))
    if 'result' in capabilities:capabilities=capabilities['result']
    # A CLI and library of the same version may have different optional features.
    # Verify both actual executable capabilities, not a caller-supplied label.
    sys.path.insert(0,str(ROOT/'bindings/python'))
    from raster_engine_lab import Engine
    with Engine(library.resolve()) as engine:
        library_capabilities=engine.call({'op':'backends'})
    if capabilities!=library_capabilities:raise SystemExit('CLI and library backend capabilities differ.')
    out=args.output_dir.resolve()
    try:out.mkdir(parents=True,exist_ok=False)
    except FileExistsError:raise SystemExit('Package output directory must be new; preserve prior artifacts and choose a new destination.')
    cache=Path(os.environ.get('SKARVE_BUILD_CACHE',Path.home()/'.cache/skarve-build'));cache.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='package-',dir=cache) as temp:
        stage=Path(temp);notice_dir=stage/'notices';inventory=notice_inventory(notice_dir,library,capabilities['exactextract'])
        system_inventory=local_system_runtime_inventory(library,inventory,notice_dir) if args.local_use_only else system_runtime_inventory(library,inventory)
        if args.local_use_only:
            inventory['runtime']='Observed local runtime; see SYSTEM_RUNTIME.json. Not qualified for binary redistribution.'
            inventory['runtime_recipe']='Use the installed distribution packages recorded in SYSTEM_RUNTIME.json.'
        inventory_path=out/'DEPENDENCIES.json';inventory_path.write_text(json.dumps(inventory,indent=2)+'\n')
        manifest={'product':'Skarve','publication_control':'Owner approval required before initial distribution','version':VERSION,'source_commit':head,'label':args.label,'library_sha256':library_sha,'cli_sha256':binary_sha,'runtime':RUNTIME,'python_distribution':'skarve-engine','python_import':'skarve','npm_package':'@skarve/engine','distribution':'GitHub Release assets; no npm or PyPI registry publication implied.','project_license':'Apache-2.0','dependency_inventory_sha256':sha(inventory_path),
                  'source_worktree_clean':True,'source_provenance':'clean_git_checkout' if checkout else 'verified_source_distribution',
                  'native_source_sha256':{str(f.relative_to(ROOT)):sha(f) for f in sorted((ROOT/'src').glob('*.rs'))+[ROOT/'Cargo.toml',ROOT/'Cargo.lock']},'source_files_sha256':source_hashes}
        manifest['local_use_only']=args.local_use_only
        manifest['distribution_qualified']=False
        if args.local_use_only:
            manifest['runtime']='Observed local Linux runtime; see SYSTEM_RUNTIME.json. Not the qualified release-host assertion.'
            manifest['distribution']='Local installation and testing only; not cleared for binary redistribution.'
        manifest['bulk_abi_version']=1
        manifest['system_runtime_inventory_sha256']=hashlib.sha256((json.dumps(system_inventory,indent=2)+'\n').encode()).hexdigest()
        manifest['policies']=['native_grid_planar_fractional','strict_selected_v1','hm_demographics_ordered_v1','hm_straight_lonlat_spherical_v1']
        manifest['backends']=capabilities
        manifest['source_interpretation']='skarve_normalized_f64_v1'
        if capabilities['exactextract']:
            manifest['policies'].extend(['exactextract_fractional_v030', 'exactextract_rasterio_v030'])
            manifest['optional_source_interpretations']=['skarve_normalized_f64_v1','rasterio_unscaled_raw_v030']
            manifest['optional_exactextract']=json.loads((ROOT/'native/exactextract/upstream.json').read_text())
        def native_files(destination,include_cli=False):
            destination.mkdir(parents=True);shutil.copy2(library,destination/'libraster_engine.so')
            if sha(destination/'libraster_engine.so')!=library_sha:raise SystemExit('Native library changed while copying.')
            copy_verified('include/skarve_bulk.h',destination/'skarve_bulk.h',source_hashes)
            if include_cli:
                shutil.copy2(binary,destination/'skarve')
                if sha(destination/'skarve')!=binary_sha:raise SystemExit('Native CLI changed while copying.')
            (destination/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n');shutil.copy2(inventory_path,destination/'DEPENDENCIES.json');shutil.copytree(notice_dir,destination/'notices')
            for name in ['LICENSE','NOTICE']:copy_verified(name,destination/name,source_hashes)
            (destination/'SYSTEM_RUNTIME.json').write_text(json.dumps(system_inventory,indent=2)+'\n')
            copy_verified_tree('third_party/notices/system',destination/'notices/system',source_hashes)
        python=stage/'python';python.mkdir()
        for name in ['pyproject.toml','setup.py','raster_engine_lab.py','skarve_bulk.py']:copy_verified(Path('bindings/python')/name,python/name,source_hashes)
        for name in ['LICENSE','NOTICE']:copy_verified(name,python/name,source_hashes)
        (python/'MANIFEST.in').write_text('recursive-include skarve/_native *\n')
        copy_verified_tree('bindings/python/skarve',python/'skarve',source_hashes);native_files(python/'skarve/_native',True)
        run([sys.executable,'-m','build','--wheel','--no-isolation','--outdir',out,python],env=os.environ|{'SOURCE_DATE_EPOCH':'1789171200'})
        node=stage/'node';node.mkdir()
        for name in ['index.mjs','bulk.mjs','index.d.ts','package.json','README.md']:copy_verified(Path('bindings/node')/name,node/name,source_hashes)
        for name in ['LICENSE','NOTICE']:copy_verified(name,node/name,source_hashes)
        for name in ['koffi','@koromix/koffi-linux-x64']:shutil.copytree(ROOT/'bindings/node/node_modules'/name,node/'node_modules'/name)
        native_files(node/'native/linux-x64');run(['npm','pack','--offline','--ignore-scripts','--pack-destination',out],cwd=node)
        cli=stage/f'skarve-{VERSION}-linux-x86_64';(cli/'bin').mkdir(parents=True);(cli/'libexec').mkdir();shutil.copy2(binary,cli/'libexec/skarve-native');native_files(cli/'lib')
        if sha(cli/'libexec/skarve-native')!=binary_sha:raise SystemExit('Native CLI changed while copying.')
        wrapper='''#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if ldd "$root/libexec/skarve-native" 2>&1 | grep -q 'not found'; then
  printf '%s\\n' 'Skarve native runtime is incomplete.' 'Supported: Ubuntu 24.04 x86-64, GDAL 3.8.4, libdeflate 1.19.' '''+repr(RECIPE)+''' >&2
  exit 1
fi
exec "$root/libexec/skarve-native" "$@"
'''
        (cli/'bin/skarve').write_text(wrapper);(cli/'bin/skarve').chmod(0o755)
        copy_verified_tree('docs',cli/'docs',source_hashes)
        copy_verified_tree('examples',cli/'examples',source_hashes)
        (cli/'third_party').mkdir()
        (cli/'third_party/SYSTEM_RUNTIME.json').write_text(json.dumps(system_inventory,indent=2)+'\n')
        copy_verified_tree('third_party/notices/system',cli/'third_party/notices/system',source_hashes)
        (cli/'README.md').write_text('Skarve '+VERSION+'\n\nStart with [installation](docs/INSTALL.md) and [workflows](docs/WORKFLOWS.md).\n')
        archive=out/f'skarve-{VERSION}-linux-x86_64.tar.gz'
        with archive.open('wb') as stream,gzip.GzipFile(fileobj=stream,mode='wb',mtime=0) as compressed,tarfile.open(fileobj=compressed,mode='w') as tar:
            def normalize(info):info.uid=info.gid=0;info.uname=info.gname='';info.mtime=0;return info
            tar.add(cli,arcname=cli.name,filter=normalize)
        source_archive=out/f'skarve-{VERSION}-source.tar.gz'
        with source_archive.open('wb') as stream,gzip.GzipFile(fileobj=stream,mode='wb',mtime=0) as compressed,tarfile.open(fileobj=compressed,mode='w') as tar:
            def normalize_source(info):info.uid=info.gid=0;info.uname=info.gname='';info.mtime=0;return info
            for file in relevant:tar.add(file,arcname=f'skarve-{VERSION}/'+str(file.relative_to(ROOT)),filter=normalize_source)
            payload=json.dumps({'source_commit':head,'source_files_sha256':source_hashes},indent=2).encode()+b'\n'
            info=tarfile.TarInfo(f'skarve-{VERSION}/SOURCE_MANIFEST.json');info.size=len(payload);info.mode=0o644;tar.addfile(info,io.BytesIO(payload))
        artifacts=[{'name':f.name,'bytes':f.stat().st_size,'sha256':sha(f)} for f in sorted(out.iterdir()) if f.suffix in ['.whl','.tgz'] or f.name.endswith('.tar.gz')]
        manifest['artifacts']=artifacts;manifest['build_tools']={tool:run(cmd).strip() for tool,cmd in [('python',[sys.executable,'--version']),('node',['node','--version']),('npm',['npm','--version'])]}
        assert source_hashes=={str(f.relative_to(ROOT)):sha(f) for f in relevant},'Package-relevant sources changed during assembly'
        (out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n');(out/'SHA256SUMS').write_text(''.join(f"{x['sha256']}  {x['name']}\n" for x in artifacts))
        assert sha(library)==library_sha and sha(binary)==binary_sha,'Native artifacts changed during packaging'
        print(json.dumps({'output':str(out),'publication_control':'Owner approval required before initial distribution','artifacts':artifacts},indent=2))
if __name__=='__main__':main()
