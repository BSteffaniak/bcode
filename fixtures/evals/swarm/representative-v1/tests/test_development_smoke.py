"""Development-only smoke controls; never open or grade held-out workspaces."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import tomllib
import unittest

BASE = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


oracle = load('development_oracle', BASE/'oracles'/'grade.py')
reference = load('development_reference', BASE/'controls'/'reference.py')


class DevelopmentSmokeTests(unittest.TestCase):
    def test_local_and_parallel_references_and_negative_controls(self):
        for case in ('train-local', 'train-parallel'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp:
                workspace = Path(temp)/case
                shutil.copytree(BASE/'workspaces'/case, workspace)
                self.assertFalse(oracle.grade(workspace, case))
                reference.produce(workspace, case)
                self.assertTrue(oracle.grade(workspace, case))
                answer = (workspace/'answer.json').read_bytes()
                (workspace/'answer.json').write_text('{"result":true}')
                self.assertFalse(oracle.grade(workspace, case))
                (workspace/'answer.json').write_bytes(answer)
                original = (workspace/'input.json').read_bytes()
                (workspace/'input.json').write_text('{}')
                self.assertFalse(oracle.grade(workspace, case))
                (workspace/'input.json').write_bytes(original)
                (workspace/'app.py').write_text('import sys; sys.exit(0)\n')
                self.assertFalse(oracle.grade(workspace, case))

    def test_manifest_uses_existing_training_tasks_and_equal_workflow_budgets(self):
        manifest = BASE.parent/'useful-swarm-measure-v1-development.toml'
        suite = tomllib.loads(manifest.read_text())
        self.assertEqual(suite['run']['repetitions'], 1)
        self.assertFalse(suite['run']['fail_fast'])
        self.assertTrue(suite['metadata']['require_paired_quality'])
        self.assertEqual([c['id'] for c in suite['cases']], ['train-local','train-parallel'])
        self.assertEqual([v['id'] for v in suite['variants']], ['direct','single','baseline-auto','adaptive-auto'])
        workflow = [v['metadata']['workflow'] for v in suite['variants'] if v['executor'] == 'workflow']
        for key in ('binary','stop_condition','max_iterations','concurrency','worker_attempts'):
            self.assertEqual(workflow[0][key],workflow[1][key])
        for variant in suite['variants']:
            config = tomllib.loads(variant['config_toml'])
            self.assertEqual(config['model']['profile'],'sol')
            self.assertEqual(config['model']['auth_pool'],'openai')
            self.assertEqual(config['model']['auth_profile'],'openai-2')
            self.assertEqual(variant['model'],'gpt-6.1-sol')
            self.assertTrue(config['observability']['persist_model_requests'])
            self.assertNotIn('pricing',config)
        for case in suite['cases']:
            self.assertTrue((BASE.parent/case['fixture']).is_dir())
            self.assertEqual(case['judges'][0]['command'],case['metadata']['workflow_oracle']['oracle_command'])
            self.assertTrue(case['judges'][0]['required'])
            self.assertNotIn('expected_json',case['metadata']['workflow_oracle'])
        self.assertEqual(workflow[1]['swarm_policy'],'baseline')
        self.assertEqual(workflow[2]['swarm_policy'],'adaptive-v1')
        self.assertNotIn('swarm_policy',workflow[0])
        self.assertIn('typed',suite['metadata']['adaptive_admission'])


if __name__ == '__main__':
    unittest.main()
