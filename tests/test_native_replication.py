"""Portable reproduction driver checks; no engine calls or timing experiments."""
import argparse
import copy
import importlib.util
import io
import json
import struct
import tempfile
import unittest
import zlib
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('native_replication_test_driver',ROOT/'benchmarks/native-skv/replicate.py')
rep=importlib.util.module_from_spec(spec)
spec.loader.exec_module(rep)


class NativeReplicationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.original=rep.read(rep.HERE/'FROZEN_PROGRAMME.json')

    def test_frozen_selection_counts_and_order(self):
        rep.protocol.validate_tasks(self.original)
        families={'analytical36','analytical40-1025x1031'}
        selected=rep.select_tasks(self.original,families,False)
        self.assertEqual(len(selected),336)
        self.assertEqual(selected,[t for t in self.original['tasks'] if t['dataset'] in families])
        smoke=rep.select_tasks(self.original,{'analytical36'},True)
        self.assertEqual(len(smoke),2)
        self.assertEqual({t['lane'] for t in smoke},{'native-cog','native-skv-summary'})
        self.assertEqual(len(rep.select_tasks(self.original,set(self.original['sources']),False)),588)
        with self.assertRaises(ValueError):rep.select_tasks(self.original,{'unknown'},False)

    def test_venv_interpreter_is_not_dereferenced(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);target=root/'system-python';target.write_text('placeholder')
            python=root/'venv/bin/python';python.parent.mkdir(parents=True);python.symlink_to(target)
            expected=str(python.absolute())
            self.assertEqual(rep.python_invocation(python),expected)
            self.assertNotEqual(expected,str(python.resolve()))
            with patch.object(rep.subprocess,'check_output',return_value='{}') as call:
                self.assertEqual(rep.probe_runtime(python,False)['python'],expected)
            self.assertEqual(call.call_args.args[0][0],expected)

    def test_index_group_order_is_part_of_the_frozen_contract(self):
        index={'tile_edge':64,'groups':[{'source_bands':[0,1]},{'source_bands':[2,3]}]}
        rep.validate_index(index,copy.deepcopy(index))
        for changed in [dict(index,tile_edge=256),dict(index,groups=list(reversed(index['groups']))),
                        dict(index,groups=[{'source_bands':[1,0]},{'source_bands':[2,3]}])]:
            with self.assertRaises(ValueError):rep.validate_index(changed,index)

    def test_missing_operations_and_controls_stay_in_denominator(self):
        tasks=rep.select_tasks(self.original,{'analytical36'},True)
        candidate=next(t for t in tasks if t['lane']=='native-skv-summary')
        result=rep.validate_results({'tasks':tasks},[{'task_id':candidate['id'],'passed':True}],Path('/unused'))
        self.assertEqual(len(result),2)
        self.assertEqual(sum(r['passed'] for r in result),0)
        self.assertEqual(sum(r['attempted'] for r in result),1)
        self.assertTrue(any(r.get('reason')=='Native COG control unavailable' for r in result))

    def test_generation_detects_replacement(self):
        with tempfile.TemporaryDirectory() as tmp:
            p=Path(tmp)/'source';p.write_bytes(b'a');before=rep.generation(p)
            replacement=Path(tmp)/'replacement';replacement.write_bytes(b'a');replacement.replace(p)
            self.assertNotEqual(before,rep.generation(p))

    def test_smoke_freeze_pins_files_and_has_a_new_runtime_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);source=copy.deepcopy(self.original['sources']['analytical36'])
            source['directory']=str(root)
            tasks=rep.select_tasks(self.original,{'analytical36'},True)
            for variant in {t['variant'] for t in tasks}:
                payload=b'fake COG: freeze validates receipt identity, workers validate decoding'
                if variant=='skv':
                    metadata={'chunk_edge':128,'band_group':36,'codec':'deflate','predictor':'byte_delta_v1',
                              'payload_layout':'band','summaries':True}
                    packed=zlib.compress(json.dumps(metadata).encode());header=bytearray(16384)
                    struct.pack_into('<I',header,24,len(packed));header[64:64+len(packed)]=packed;payload=bytes(header)
                path=root/variant;path.write_bytes(payload)
                source['variants'][variant].update(path=variant,bytes=len(payload),sha256=rep.sha(path))
            prepared=root/'prepared.json';rep.write(prepared,{'sources':[source]})
            args=argparse.Namespace(prepared=[prepared],datasets='analytical36',smoke=True,python=root/'venv/bin/python',output=root/'frozen')
            runtime={'python':str(args.python),'library_sha256':'new-library','actual_versions':{'skarve':'new-candidate'}}
            with patch.object(rep,'probe_runtime',return_value=runtime),redirect_stdout(io.StringIO()):rep.freeze(args)
            frozen=rep.read(args.output/'freeze.json')
            self.assertEqual(frozen['counts'],{'operations':2,'smoke_only':True})
            self.assertIsNone(frozen['source_commit'])
            self.assertEqual(frozen['historical_source_commit'],self.original['source_commit'])
            self.assertFalse(frozen['runtime']['same_library_bytes_as_historical'])
            self.assertEqual(len(frozen['objects']),2)
            source['logical']['bands'][0]['scale']=2
            prepared2=root/'changed.json';rep.write(prepared2,{'sources':[source]});args.prepared=[prepared2];args.output=root/'rejected'
            with self.assertRaisesRegex(ValueError,'Logical pixels'):rep.freeze(args)


if __name__=='__main__':unittest.main()
