#!/usr/bin/env python3
"""Offline study tests: no provider, daemon or actual trial execution."""
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor
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

    def test_context_pair_maps_both_arms_and_rejects_split_admissions(self):
        trials = [{'id': arm, 'arm': arm, 'report_arm': arm, 'adapter': 'context',
                   'admission_pair_id': 'pair', 'job_file': 'job', 'job_sha256': 'hash',
                   'report_file': 'pilot', 'receipt_file': 'shared', 'case_id': 'c', 'repetition': 0,
                   'metric': 'coding_task_success'} for arm in ('off', 'on')]
        study.validate_context_pairs(trials)
        report = {'arms': [{'arm': arm, 'gap': None, 'oracle': {'status': 'pass'},
                   'capture': {'status': 'captured', 'outcome': 'succeeded',
                               'telemetry': {'optional_operations_complete': True, 'inference_calls': calls}},
                   'hook_evidence': {'all_required_satisfied': True}} for arm, calls in (('off', 0), ('on', 1))]}
        self.assertEqual(study.summarize(report, trials[0])['inference_calls'], 0)
        self.assertEqual(study.summarize(report, trials[1])['inference_calls'], 1)
        trials[1]['receipt_file'] = 'different'
        with self.assertRaises(study.cli.PilotError):
            study.validate_context_pairs(trials)

    def test_context_pair_dispatches_once_and_maps_two_results(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            job = root / 'job.json'
            job.write_text('{}')
            corpus = root / 'corpus.json'
            corpus.write_text('{}')
            report_path = root / 'pilot.json'
            trials = [{'id': arm, 'arm': arm, 'report_arm': arm, 'adapter': 'context',
                       'admission_pair_id': 'pair', 'job_file': str(job), 'job_sha256': study.cli.digest(job),
                       'report_file': str(report_path), 'receipt_file': str(root / 'receipt.json'),
                       'result_file': str(root / (arm + '-result.json')), 'case_id': 'c', 'repetition': 0,
                       'feature': 'context_reranking', 'metric': 'coding_task_success'} for arm in ('off', 'on')]
            schedule = {'trials': trials, 'corpus_file': str(corpus), 'corpus_sha256': study.cli.digest(corpus),
                        'subject': {'executable': str(job), 'sha256': study.cli.digest(job)}}
            def complete(*args, **kwargs):
                report_path.write_text(json.dumps({'arms': [{'arm': arm, 'gap': None,
                    'oracle': {'status': 'pass'}, 'capture': {'status': 'captured', 'outcome': 'succeeded',
                    'telemetry': {'optional_operations_complete': True}}, 'terminal_proof': {'verified': True}}
                    for arm in ('off', 'on')]}))
                return mock.Mock(returncode=0)
            with mock.patch.object(study.subprocess, 'run', side_effect=complete) as launch:
                study.run(schedule, 2)
                launch.assert_called_once()
                self.assertTrue(all(Path(t['result_file']).exists() for t in trials))
                study.run(schedule, 1)
                launch.assert_called_once()
            incomplete = json.loads(report_path.read_text())
            incomplete['arms'][1]['terminal_proof']['verified'] = False
            report_path.write_text(json.dumps(incomplete))
            with mock.patch.object(study.subprocess, 'run') as launch:
                with self.assertRaises(study.cli.PilotError):
                    study.run(schedule, 1)
                launch.assert_not_called()

    def pair_fixture(self, root):
        job = root / 'job.json'
        job.write_text('{}')
        corpus = root / 'corpus.json'
        corpus.write_text('{}')
        trials = [{'id': arm, 'arm': arm, 'report_arm': arm, 'adapter': 'context',
                   'admission_pair_id': 'pair', 'job_file': str(job), 'job_sha256': study.cli.digest(job),
                   'report_file': str(root / 'pilot.json'), 'receipt_file': str(root / 'receipt.json'),
                   'result_file': str(root / (arm + '-result.json')), 'case_id': 'c', 'repetition': 0,
                   'feature': 'context_reranking', 'metric': 'coding_task_success'} for arm in ('off', 'on')]
        return {'trials': trials, 'corpus_file': str(corpus), 'corpus_sha256': study.cli.digest(corpus),
                'subject': {'executable': str(job), 'sha256': study.cli.digest(job)}}

    def settled_pair(self):
        return {'arms': [{'arm': arm, 'gap': None, 'oracle': {'status': 'pass'},
                         'capture': {'status': 'captured', 'outcome': 'succeeded',
                                     'telemetry': {'optional_operations_complete': True}},
                         'terminal_proof': {'verified': True}} for arm in ('off', 'on')]}

    def test_context_settled_rebind_and_unresolved_rejection_no_dispatch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schedule = self.pair_fixture(root)
            trial = schedule['trials'][0]
            study.cli.write_json(trial['receipt_file'], {'id': 'pair', 'job_sha256': trial['job_sha256']})
            report = self.settled_pair()
            Path(trial['report_file']).write_text(json.dumps(report))
            with mock.patch.object(study.cli, 'external_path'), mock.patch.object(study.subprocess, 'run') as launch:
                rebound = study.rebind_resume(schedule, root / 'rebound.json')
                self.assertEqual(rebound['resume_reconciliation'], 'retained_terminal_captures_no_replay')
                self.assertTrue(all(Path(t['result_file']).exists() for t in schedule['trials']))
                report['arms'][1]['terminal_proof']['verified'] = False
                Path(trial['report_file']).write_text(json.dumps(report))
                with self.assertRaises(study.cli.PilotError):
                    study.rebind_resume(schedule, root / 'unresolved.json')
                self.assertFalse((root / 'unresolved.json').exists())
                launch.assert_not_called()

    def test_concurrent_pair_receipt_has_one_winner_one_explicit_conflict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schedule = self.pair_fixture(root)
            barrier = threading.Barrier(2)
            loser = threading.Event()
            write_json = study.cli.write_json
            def claim(path, value):
                barrier.wait(timeout=5)
                write_json(path, value)
            def complete(*args, **kwargs):
                self.assertTrue(loser.wait(timeout=5))
                Path(schedule['trials'][0]['report_file']).write_text(json.dumps(self.settled_pair()))
                return mock.Mock(returncode=0)
            def attempt():
                try:
                    study.run(schedule, 1)
                    return 'winner'
                except study.cli.PilotError as error:
                    loser.set()
                    return str(error)
            with mock.patch.object(study.cli, 'write_json', side_effect=claim), \
                    mock.patch.object(study.subprocess, 'run', side_effect=complete) as launch:
                with ThreadPoolExecutor(max_workers=2) as pool:
                    futures = [pool.submit(attempt) for _ in range(2)]
                    results = [future.result(timeout=10) for future in futures]
                self.assertCountEqual(results, ['winner', 'admission conflict; receipt already claimed; no dispatch'])
                launch.assert_called_once()

    def test_unsettled_adapters_block_later_filtered_admission(self):
        for adapter in ('normal', 'eval', 'navigation'):
            with self.subTest(adapter=adapter), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                schedule = self.pair_fixture(root)
                prior = dict(schedule['trials'][0], adapter=adapter, id='prior', feature='excluded')
                later = dict(schedule['trials'][1], adapter='normal', id='later', feature='selected',
                             receipt_file=str(root / 'later-receipt.json'))
                schedule['trials'] = [prior, later]
                study.cli.write_json(prior['receipt_file'], {'id': 'pair', 'job_sha256': prior['job_sha256']})
                Path(prior['report_file']).write_text(json.dumps({'gap': 'retained_failure',
                    'capture': {'status': 'captured', 'outcome': 'failed'}}))
                # Even an already-reduced result and a filter excluding prior work cannot authorize continuation.
                Path(prior['result_file']).write_text('{}')
                with mock.patch.object(study.subprocess, 'run') as launch, mock.patch.object(study.cli, 'command') as command:
                    with self.assertRaises(study.cli.PilotError):
                        study.run(schedule, 1, features=['selected'])
                    launch.assert_not_called()
                    command.assert_not_called()
                    self.assertFalse(Path(later['receipt_file']).exists())

    def test_adapter_failure_retains_safe_diagnostic_and_admitted_gap(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schedule = self.pair_fixture(root)
            def failure(*args, **kwargs):
                kwargs['stderr'].write(b'Pilot preflight missing source_order; consumer not started.\n')
                return mock.Mock(returncode=1)
            with mock.patch.object(study.subprocess, 'run', side_effect=failure) as launch:
                result = study.run(schedule, 1)
                launch.assert_called_once()
            self.assertTrue(all(t['gap'] == 'admitted_without_report' for t in result['trials']))
            diagnostic = study.cli.read_json(root / 'receipt.diagnostic.json')
            self.assertTrue(diagnostic['consumer_not_started'])
            self.assertTrue((root / 'receipt.json').exists())
            with mock.patch.object(study.subprocess, 'run') as launch:
                with self.assertRaises(study.cli.PilotError):
                    study.run(schedule, 1)
                launch.assert_not_called()
            with tempfile.TemporaryFile() as stream:
                stream.write(b'Authorization: Bearer secret\n' + b'x' * 5000)
                safe = study.adapter_diagnostic(stream, 1)
                self.assertNotIn('secret', safe['stderr'])
                self.assertTrue(safe['stderr_truncated'])
                self.assertFalse(safe['consumer_not_started'])

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
