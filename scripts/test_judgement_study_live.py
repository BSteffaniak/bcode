#!/usr/bin/env python3
"""Offline study tests: no provider, daemon or actual trial execution."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('study_driver', Path(__file__).with_name('judgement-study-live.py'))
study = importlib.util.module_from_spec(spec)
spec.loader.exec_module(study)


class StudyTests(unittest.TestCase):
    def test_failed_task_is_result_and_missing_truth_is_gap(self):
        trial = {'id': 'x', 'adapter': 'normal', 'metric': 'coding_task_success'}
        report = {'oracle': {'status': 'fail'}, 'capture': {'status': 'captured',
                  'telemetry': {'optional_operations_complete': True, 'inference_calls': 1}}}
        result = study.summarize(report, trial)
        self.assertEqual(result['truth'], 'fail')
        self.assertIsNone(result['gap'])
        report['oracle'] = {}
        self.assertEqual(study.summarize(report, trial)['gap'], 'independent_metric_unavailable')

    def test_native_navigation_never_uses_harness_benefit(self):
        result = study.summarize({'benefit': True, 'adapted_top1_relevant': True},
                                {'id': 'n', 'adapter': 'navigation', 'metric': 'native_navigation'})
        self.assertIsNone(result['truth'])
        self.assertIsNotNone(result['gap'])

    def test_paired_ties_and_partial_counts(self):
        with tempfile.TemporaryDirectory() as root:
            trials = []
            for arm in ('off', 'on'):
                path = Path(root) / (arm + '.json')
                path.write_text(json.dumps({'truth': 'pass', 'gap': None, 'optional_complete': True,
                                            'latency_ms': 100 if arm == 'off' else 140}))
                trials.append({'feature': 'progress_monitoring', 'case_id': 'c', 'repetition': 0,
                               'arm': arm, 'result_file': str(path)})
            report = study.reduce({'trials': trials, 'corpus_sha256': 'pinned'})
            group = report['groups']['progress_monitoring']
            self.assertEqual(group['paired_ties'], 1)
            self.assertEqual(group['paired_wins'], 0)
            self.assertEqual(group['latency_ratios'], [1.4])
            self.assertFalse(report['acceptance_claimed'])
            Path(trials[0]['result_file']).write_text(json.dumps({'truth': 'pass', 'optional_complete': False}))
            self.assertEqual(study.reduce({'trials': trials, 'corpus_sha256': 'pinned'})['groups']['progress_monitoring']['gaps'], 1)

    def test_contrasts_do_not_overwrite_and_nontriggered_truth_stays_itt(self):
        with tempfile.TemporaryDirectory() as root:
            trials = []
            for contrast in ('single_vs_all_off', 'all_on_vs_leave_out'):
                for arm in ('off', 'on'):
                    path = Path(root) / (contrast + arm + '.json')
                    path.write_text(json.dumps({'truth': 'pass', 'task_truth': 'pass', 'gap': None,
                        'optional_complete': True, 'hook_application_satisfied': contrast == 'single_vs_all_off'}))
                    trials.append({'id': contrast + arm, 'feature': 'recommendations', 'case_id': 'same',
                        'repetition': 0, 'contrast_id': contrast, 'treatment': contrast + arm,
                        'arm': arm, 'result_file': str(path)})
            report = study.reduce({'trials': trials, 'corpus_sha256': 'pin'})
            group = report['groups']['recommendations']
            self.assertEqual(group['paired_ties'], 2)
            self.assertEqual(group['qualified_ties'], 1)
            self.assertEqual(group['intent_to_treat_pass'], 4)
            self.assertEqual(len(report['trials']), 4)
            path = Path(trials[-1]['result_file'])
            value = json.loads(path.read_text())
            value['gap'] = 'trigger_not_observed'
            path.write_text(json.dumps(value))
            group = study.reduce({'trials': trials, 'corpus_sha256': 'pin'})['groups']['recommendations']
            self.assertEqual(group['intent_to_treat_pass'], 4)
            self.assertEqual(group['gaps'], 1)

    def test_raw_eval_capture_needs_scoped_independent_oracle(self):
        trial = {'id': 'e', 'adapter': 'eval', 'metric': 'coding_task_success'}
        raw = {'scope': 'normal_eval_attempt', 'status': 'captured', 'outcome': 'failed',
               'telemetry': {'optional_operations_complete': True, 'inference_calls': 1}}
        self.assertEqual(study.summarize(raw, trial)['gap'], 'independent_eval_oracle_artifact_required')
        with mock.patch.object(study, 'oracle_artifact', return_value={'passed': False, 'capture_complete': True}):
            trial['oracle_artifact'] = {'path': '/explicit/oracle', 'sha256': 'pinned'}
            result = study.summarize(raw, trial)
            self.assertEqual(result['truth'], 'fail')
            self.assertIsNone(result['gap'])
        with mock.patch.object(study, 'oracle_artifact', return_value={'passed': True, 'capture_complete': False}):
            result = study.summarize(raw, trial)
            self.assertEqual(result['task_truth'], 'pass')
            self.assertEqual(result['gap'], 'independent_eval_capture_incomplete')

    def test_native_navigation_wrapper_has_independent_metrics(self):
        report = {'native_metric': {'scope': 'product_native', 'status': 'pass'},
                  'capture': {'telemetry': {'optional_operations_complete': True, 'inference_calls': 3}},
                  'hook_evidence': {'all_required_satisfied': True}}
        result = study.summarize(report, {'id': 'n', 'adapter': 'navigation', 'metric': 'native_navigation'})
        self.assertEqual(result['truth'], 'pass')
        self.assertTrue(result['optional_complete'])
        self.assertEqual(result['inference_calls'], 3)

    def test_resume_rebind_requires_terminal_receipt_proof_no_replay(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            job = root / 'job.json'
            job.write_text('{}')
            receipt = root / 'receipt.json'
            receipt.write_text(json.dumps({'id': 'x', 'job_sha256': study.cli.digest(job)}))
            corpus = root / 'corpus.json'
            corpus.write_text('{}')
            report = root / 'report.json'
            trial = {'id': 'x', 'adapter': 'normal', 'metric': 'coding_task_success',
                     'receipt_file': str(receipt), 'job_file': str(job), 'job_sha256': study.cli.digest(job),
                     'report_file': str(report), 'result_file': str(root / 'result.json')}
            schedule = {'trials': [trial], 'corpus_file': str(corpus), 'corpus_sha256': study.cli.digest(corpus)}
            with mock.patch.object(study.cli, 'external_path'), mock.patch.object(study.subprocess, 'run') as launch:
                with self.assertRaises(study.cli.PilotError):
                    study.rebind_resume(schedule, root / 'rebound.json')
                report.write_text(json.dumps({'turn_id': 't', 'history_after': {'complete': True},
                    'gap': 'trigger_missing', 'oracle': {'status': 'pass'},
                    'capture': {'status': 'fallback_reason_unknown', 'outcome': 'succeeded', 'telemetry': {}}}))
                result = study.rebind_resume(schedule, root / 'rebound.json')
                self.assertEqual(result['resume_reconciliation'], 'retained_terminal_captures_no_replay')
                self.assertTrue(Path(trial['result_file']).exists())
                launch.assert_not_called()

    def test_unreconciled_receipt_blocks_repeat(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            corpus = root / 'corpus.json'
            corpus.write_text('{}')
            receipt = root / 'receipt.json'
            receipt.write_text('{}')
            schedule = {'corpus_file': str(corpus), 'corpus_sha256': study.cli.digest(corpus),
                        'trials': [{'receipt_file': str(receipt), 'result_file': str(root / 'result.json'),
                                    'report_file': str(root / 'missing.json'), 'id': 'x'}]}
            with mock.patch.object(study.subprocess, 'run') as launch:
                with self.assertRaises(study.cli.PilotError):
                    study.run(schedule, 1)
                launch.assert_not_called()


if __name__ == '__main__':
    unittest.main()
