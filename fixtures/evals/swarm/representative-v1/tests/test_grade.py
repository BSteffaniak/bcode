"""Offline behavioral controls; no provider, daemon, or workflow launches."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import tomllib
import unittest

BASE = Path(__file__).resolve().parents[1]


def load(name,path):
    spec = importlib.util.spec_from_file_location(name,path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


oracle = load('representative_oracle',BASE/'oracles'/'grade.py')
reference = load('representative_reference',BASE/'controls'/'reference.py')
CASES = sorted(oracle.TESTS.keys() | oracle.REVIEW.keys())


class RepresentativeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def workspace(self,case,good=False):
        path = self.root/case
        shutil.copytree(BASE/'workspaces'/case,path,dirs_exist_ok=True)
        if good:
            reference.produce(path,case)
        return path

    def test_all_eight_references_pass(self):
        for case in CASES:
            with self.subTest(case=case):
                self.assertTrue(oracle.grade(self.workspace(case,True),case))

    def test_all_eight_initial_subjects_fail_even_with_correct_artifacts(self):
        for case in CASES:
            with self.subTest(case=case):
                good = self.workspace(case,True)
                answer = (good/'answer.json').read_bytes()
                # Restore original buggy source, but retain the known-good artifact.
                for source in (BASE/'workspaces'/case).glob('*.py'):
                    (good/source.name).write_bytes(source.read_bytes())
                if case in oracle.REVIEW:
                    (good/'answer.json').write_text('{"findings":[]}')
                else:
                    (good/'answer.json').write_bytes(answer)
                self.assertFalse(oracle.grade(good,case))

    def test_every_reference_rejects_missing_wrong_and_mutated_inputs(self):
        for case in CASES:
            with self.subTest(case=case):
                workspace = self.workspace(case,True)
                answer = (workspace/'answer.json').read_bytes()
                (workspace/'answer.json').unlink()
                self.assertFalse(oracle.grade(workspace,case))
                (workspace/'answer.json').write_text('{"result":true}')
                self.assertFalse(oracle.grade(workspace,case))
                (workspace/'answer.json').write_bytes(answer)
                (workspace/'input.json').write_text('{}')
                self.assertFalse(oracle.grade(workspace,case))

    def test_crash_exit_kill_loop_and_forged_pass_fail(self):
        for source in (
            'import sys; sys.exit(0)\n',
            'import os; os._exit(0)\n',
            'import os, signal; os.kill(os.getpid(),signal.SIGKILL)\n',
            'raise RuntimeError("crash")\n',
            'while True: pass\n',
            'print("passed",flush=True)\nimport os; os._exit(0)\n',
            'def evaluate(rows): return True\n',
            'def evaluate(rows): rows.clear(); return 5\n',
        ):
            workspace = self.workspace('train-local',True)
            (workspace/'app.py').write_text(source)
            with self.subTest(source=source):
                self.assertFalse(oracle.grade(workspace,'train-local',timeout=0.1))

    def test_artifact_protocol_types_duplicates_symlinks_and_size(self):
        workspace = self.workspace('train-local',True)
        for answer in ('{"result":5.0}', '{"result":5,"result":5}',
                       '{"result":5,"passed":true}', 'x'*20000):
            (workspace/'answer.json').write_text(answer)
            self.assertFalse(oracle.grade(workspace,'train-local'))
        (workspace/'answer.json').unlink()
        target = self.root/'answer.json'
        target.write_text('{"result":5}')
        (workspace/'answer.json').symlink_to(target)
        self.assertFalse(oracle.grade(workspace,'train-local'))

    def test_review_false_positives_missing_findings_and_source_mutation(self):
        for case in oracle.REVIEW:
            workspace = self.workspace(case,True)
            answer = json.loads((workspace/'answer.json').read_text())
            answer['findings'].append({'file':'safe.py','function':'unique','category':'imaginary','severity':'medium','explanation':'invented'})
            (workspace/'answer.json').write_text(json.dumps(answer))
            self.assertFalse(oracle.grade(workspace,case))
            reference.produce(workspace,case)
            answer = json.loads((workspace/'answer.json').read_text())
            answer['findings'].pop()
            (workspace/'answer.json').write_text(json.dumps(answer))
            self.assertFalse(oracle.grade(workspace,case))
            reference.produce(workspace,case)
            (workspace/'safe.py').write_text('# edited\n')
            self.assertFalse(oracle.grade(workspace,case))

    def test_recovery_retry_exhaustion_and_no_failed_commit(self):
        workspace = self.workspace('holdout-recovery',True)
        self.assertTrue(oracle.grade(workspace,'holdout-recovery'))
        response = oracle.observe(workspace,{'rows':[],'failures':3})
        self.assertTrue(oracle.valid_observation(response,'TransientError',error=True))
        response = oracle.observe(workspace,{'rows':[],'failures':2})
        self.assertTrue(oracle.valid_observation(response,{'published':{},'attempts':3,'commits':[{}]}))
        initial = BASE/'workspaces'/'holdout-recovery'/'publish.py'
        (workspace/'publish.py').write_bytes(initial.read_bytes())
        self.assertFalse(oracle.grade(workspace,'holdout-recovery'))

    def test_worker_protocol_schema_is_host_adjudicated(self):
        good = {'schema_version':1,'kind':'value','value':5,'input_preserved':True}
        self.assertTrue(oracle.valid_observation(good,5))
        for patch in ({'schema_version':True}, {'schema_version':2},
                      {'input_preserved':False}, {'value':5.0}, {'kind':'passed'},
                      {'passed':True}):
            response = {**good,**patch}
            self.assertFalse(oracle.valid_observation(response,5))
        self.assertFalse(oracle.valid_observation({'passed':True},5))

    def test_manifests_disjoint_family_split_and_grading_assets_not_copied(self):
        sets = []
        for split in ('train','holdout'):
            manifest = BASE.parent/f'representative-v1-{split}.toml'
            suite = tomllib.loads(manifest.read_text())
            ids = {case['id'] for case in suite['cases']}
            self.assertEqual(len(ids),4)
            self.assertEqual(len(suite['variants']),4)
            self.assertEqual(suite['run']['repetitions'],3)
            self.assertNotIn('pricing',suite['metadata'])
            self.assertTrue(all('model' not in v for v in suite['variants']))
            for case in suite['cases']:
                path = BASE.parent/case['fixture']
                self.assertTrue(path.is_dir())
                self.assertNotIn(BASE/'oracles',path.parents)
                self.assertFalse((path/'answer.json').exists())
                self.assertFalse((path/'reference_sources.json').exists())
                self.assertTrue(case['judges'][0]['required'])
            sets.append(ids)
        self.assertFalse(sets[0] & sets[1])
        self.assertEqual(sets[0] | sets[1],set(CASES))


if __name__ == '__main__':
    unittest.main()
