"""Small release-boundary regressions; no compiler, package manager or network."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPT=Path(__file__).resolve().parents[1]/'scripts/package_release.py'
SPEC=importlib.util.spec_from_file_location('release_packaging_under_test',SCRIPT)
package=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(package)


class ReleaseBoundaries(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory(prefix='skarve-package-boundary-')
        self.root=Path(self.temporary.name)/'source';self.root.mkdir()
        self.override=patch.object(package,'ROOT',self.root);self.override.start()
        self.addCleanup(self.override.stop);self.addCleanup(self.temporary.cleanup)
        self.version=patch.object(package,'VERSION','0.1.0-beta.1');self.version.start();self.addCleanup(self.version.stop)
        fixtures={'Cargo.toml':'[package]\nname="skarve"\nversion="0.1.0-beta.1"\n',
                  'Cargo.lock':'# generated test fixture\n','scripts/package_release.py':'# fixture\n',
                  'LICENSE':'project license fixture\n','NOTICE':'project notice fixture\n',
                  'bindings/python/pyproject.toml':'[project]\nname="skarve-engine"\nversion="0.1.0b1"\n',
                  'bindings/python/skarve/__init__.py':'__version__ = "0.1.0b1"\n',
                  'bindings/node/package.json':json.dumps({'name':'@skarve/engine','version':'0.1.0-beta.1'}),
                  'docs/INSTALL.md':'generated test documentation\n'}
        for name,text in fixtures.items():
            path=self.root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(text)
        self.hashes={name:package.sha(self.root/name) for name in fixtures}
        self.record={'source_commit':'a'*40,'source_files_sha256':self.hashes}
        self.write_record()

    def write_record(self):
        (self.root/'SOURCE_MANIFEST.json').write_text(json.dumps(self.record))

    def test_source_archive_packages_without_git(self):
        checkout,head,files,hashes=package.source_snapshot()
        self.assertFalse(checkout);self.assertEqual(head,'a'*40);self.assertEqual(hashes,self.hashes)
        self.assertNotIn(self.root/'SOURCE_MANIFEST.json',files)

    def test_git_initialized_source_archive_regenerates_one_manifest(self):
        (self.root/'.git').mkdir()
        def git(command,**_):
            if command[1]=='status':return ''
            if command[1]=='rev-parse':return 'b'*40+'\n'
            if command[1]=='ls-files':return '\0'.join([*self.hashes,'SOURCE_MANIFEST.json'])+'\0'
            raise AssertionError(command)
        with patch.object(package,'run',git):checkout,head,files,hashes=package.source_snapshot()
        self.assertTrue(checkout);self.assertEqual(head,'b'*40)
        self.assertNotIn('SOURCE_MANIFEST.json',hashes)
        self.assertNotIn(self.root/'SOURCE_MANIFEST.json',files)

    def test_archive_source_change_rejected(self):
        (self.root/'docs/INSTALL.md').write_text('modified after release')
        with self.assertRaisesRegex(SystemExit,'Source archive changed'):package.source_snapshot()

    def test_manifest_traversal_and_missing_inputs_rejected(self):
        self.record['source_files_sha256']={'../outside':'0'*64};self.write_record()
        with self.assertRaisesRegex(SystemExit,'Unsafe source manifest path'):package.source_snapshot()
        self.record['source_files_sha256']={'Cargo.toml':self.hashes['Cargo.toml']};self.write_record()
        with self.assertRaisesRegex(SystemExit,'Incomplete source manifest'):package.source_snapshot()

    def test_unlisted_package_code_and_documentation_rejected(self):
        for tree,extra in [('bindings/python/skarve','extra.py'),('docs','private-draft.md')]:
            unexpected=self.root/tree/extra;unexpected.write_text('not in the verified source archive')
            with self.assertRaisesRegex(SystemExit,'Unexpected file'):
                package.copy_verified_tree(tree,Path(self.temporary.name)/'staged'/tree,self.hashes)
            unexpected.unlink()

    def test_generated_bytecode_excluded_and_source_copied_by_verified_hash(self):
        cache=self.root/'bindings/python/skarve/__pycache__';cache.mkdir();(cache/'module.pyc').write_bytes(b'generated')
        destination=Path(self.temporary.name)/'package'
        package.copy_verified_tree('bindings/python/skarve',destination,self.hashes)
        self.assertEqual({p.name for p in destination.iterdir()},{'__init__.py'})
        self.assertEqual(package.sha(destination/'__init__.py'),self.hashes['bindings/python/skarve/__init__.py'])
        (self.root/'bindings/python/skarve/__init__.py').write_text('changed after snapshot')
        with self.assertRaisesRegex(SystemExit,'changed while copying'):
            package.copy_verified('bindings/python/skarve/__init__.py',destination/'changed.py',self.hashes)

    def test_symlink_package_input_rejected(self):
        destination=Path(self.temporary.name)/'staged'
        (self.root/'docs/link').symlink_to(self.root/'LICENSE')
        with self.assertRaisesRegex(SystemExit,'Unexpected file'):
            package.copy_verified_tree('docs',destination,self.hashes)

    def test_exact_versions_across_cli_python_node(self):
        correct='skarve 0.1.0-beta.1 (JSON protocol v1; native-grid planar)\n'
        package.validate_versions(correct)
        for wrong in ['skarve 0.1.0-beta.10','other-tool 0.1.0-beta.1']:
            with self.assertRaisesRegex(SystemExit,'CLI version'):package.validate_versions(wrong)
        (self.root/'bindings/node/package.json').write_text(json.dumps({'name':'@skarve/engine','version':'0.1.0-beta.2'}))
        with self.assertRaisesRegex(SystemExit,'Node distribution'):package.validate_versions(correct)
        (self.root/'bindings/node/package.json').write_text(json.dumps({'name':'@skarve/engine','version':'0.1.0-beta.1'}))
        (self.root/'bindings/python/skarve/__init__.py').write_text('__version__ = "0.1.0b2"\n')
        with self.assertRaisesRegex(SystemExit,'Python import version'):package.validate_versions(correct)


if __name__=='__main__':unittest.main()
