"""Local package observations must not relax the release distribution gate."""
import importlib.util
import json
from pathlib import Path
import sys
import pytest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('local_package_release', ROOT/'scripts/package_release.py')
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)

def test_strict_distribution_still_rejects_changed_system_version(monkeypatch):
    reviewed = json.loads((ROOT/'third_party/SYSTEM_RUNTIME.json').read_text())
    library = reviewed['libraries'][0]['soname']
    monkeypatch.setattr(package, 'run', lambda command, **kwargs: 'not-the-reviewed-version')
    with pytest.raises(RuntimeError, match='System package changed since notice review'):
        package.system_runtime_inventory(None, {'native_external_libraries': [{'soname': library}]})

def test_local_inventory_records_actual_versions_and_notices(tmp_path, monkeypatch):
    binary = Path('/usr/lib/x86_64-linux-gnu/libc.so.6')
    if not binary.exists():
        pytest.skip('Debian/Ubuntu local packaging qualification')
    real_run = package.run
    def current_version(command, **kwargs):
        if command[:3] == ['dpkg-query', '-W', '-f=${Version}']:
            return 'test-local-version'
        result = real_run(command, **kwargs)
        if command[:2] == ['dpkg-query', '-S']:
            result = 'diversion by example from: /lib/example.so\n' + result
        return result
    monkeypatch.setattr(package, 'run', current_version)
    inventory = package.local_system_runtime_inventory(binary, {'native_external_libraries': [
        {'soname': 'libc.so.6', 'system_path': str(binary), 'sha256': package.sha(binary), 'bundled': False}
    ]}, tmp_path)
    assert inventory['distribution_qualified'] is False
    assert inventory['packages']
    for item in inventory['packages']:
        assert item['version'] == 'test-local-version'
        assert package.sha(tmp_path/item['copyright_record']) == item['copyright_sha256']

def test_local_inventory_rejects_unknown_library_ownership(tmp_path, monkeypatch):
    def missing(*args, **kwargs):
        raise RuntimeError('No package owns this file')
    monkeypatch.setattr(package, 'run', missing)
    with pytest.raises(RuntimeError, match='Cannot identify installed system library package'):
        package.local_system_runtime_inventory(None, {'native_external_libraries': [
            {'soname': 'unowned.so', 'system_path': '/missing/unowned.so'}
        ]}, tmp_path)
