#!/usr/bin/env python3
"""Package and test Skarve as an external Rust consumer; never publish anything."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', required=True, type=Path, help='New isolated work directory')
    parser.add_argument('--target-dir', type=Path, help='Reusable Cargo cache outside the checkout')
    parser.add_argument('--offline', action='store_true', help='Use already cached registry dependencies')
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    target = (args.target_dir or work / 'target').resolve()
    spec = importlib.util.spec_from_file_location('skarve_build', ROOT / 'scripts/build.py')
    build = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(build)
    env = build.build_env(target)
    env.update(CARGO_PROFILE_DEV_DEBUG='0', CARGO_PROFILE_TEST_DEBUG='0', CARGO_INCREMENTAL='0')
    network = ['--offline'] if args.offline else []
    records = []

    def run(name, command, cwd=ROOT):
        completed = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True)
        (work / (name + '.stdout')).write_text(completed.stdout)
        (work / (name + '.stderr')).write_text(completed.stderr)
        records.append({'name': name, 'exit_code': completed.returncode,
                        'command': [str(c).replace(str(work), '$WORK').replace(str(ROOT), '$SOURCE') for c in command]})
        if completed.returncode:
            raise RuntimeError(f'{name} failed; inspect {work / (name + ".stderr")}')
        return completed.stdout

    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
    run('package', ['cargo', 'package', '--allow-dirty', '--no-verify', '--locked', *network])
    archive = target / 'package' / f'skarve-{version}.crate'
    frozen = work / archive.name
    shutil.copyfile(archive, frozen)
    with tarfile.open(frozen) as package:
        members = [m.name for m in package.getmembers()]
        package.extractall(work / 'package', filter='data')
    extracted = work / 'package' / f'skarve-{version}'
    assert (extracted / 'Cargo.toml').is_file()
    # Cargo consumers do not receive this project's source checkout or scripts.
    # They receive only the .crate extraction, normal native dependencies and
    # a separately authored application manifest/main.
    consumer = work / 'consumer'
    (consumer / 'src').mkdir(parents=True)
    (consumer / 'Cargo.toml').write_text(
        '[package]\nname="skarve-package-consumer"\nversion="0.0.0"\nedition="2024"\npublish=false\n'
        '[dependencies]\nskarve={path="../package/skarve-' + version + '", version="=' + version + '"}\n'
        'serde_json="1.0"\nanyhow="1.0"\ngdal="=0.19.0"\n')
    shutil.copyfile(extracted / 'examples/rust_skv.rs', consumer / 'src/main.rs')
    run('consumer-lock', ['cargo', 'generate-lockfile', '--manifest-path', str(consumer / 'Cargo.toml'), *network], consumer)
    result = run('consumer-run', ['cargo', 'run', '--locked', '-j', '2', '--manifest-path',
        str(consumer / 'Cargo.toml'), *network, '--', str(work / 'example-output')], consumer)
    output = json.loads(result)
    assert output['batch_rows'] == 2 and output['original_removed_before_skv_serving'] is True
    run('packaged-api-tests', ['cargo', 'test', '--locked', '-j', '2', '--manifest-path',
        str(extracted / 'Cargo.toml'), '--test', 'rust_api', '--lib', *network], extracted)
    run('packaged-doc-tests', ['cargo', 'test', '--locked', '-j', '2', '--manifest-path',
        str(extracted / 'Cargo.toml'), '--doc', *network], extracted)
    receipt = {'schema': 1, 'version': version, 'publication_attempted': False,
        'archive_sha256': hashlib.sha256(frozen.read_bytes()).hexdigest(),
        'archive_bytes': frozen.stat().st_size, 'members': members,
        'runtime': {'rustc': run('rustc-version', ['rustc', '--version']).strip(),
                    'cargo': run('cargo-version', ['cargo', '--version']).strip()},
        'consumer': {'batch_rows': output['batch_rows'], 'original_removed_before_skv_serving': True,
                     'sums': [b['fractional_sum'] for b in output['single']['bands']]},
        'operations': records}
    (work / 'RUST_PACKAGE_RECEIPT.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'receipt': str(work / 'RUST_PACKAGE_RECEIPT.json'),
                      'archive_sha256': receipt['archive_sha256'], 'consumer_sums': receipt['consumer']['sums']}))


if __name__ == '__main__':
    main()
