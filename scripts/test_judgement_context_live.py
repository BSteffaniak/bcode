"""Offline treatment and evidence contracts; no provider calls."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('context_comparison', Path(__file__).with_name('judgement-context-live.py'))
context = importlib.util.module_from_spec(spec)
spec.loader.exec_module(context)


class ContextComparisonTests(unittest.TestCase):
    def test_context_overlay_requires_explicit_request_persistence_before_import(self):
        arm = {'overlay_toml': '[observability]\npersist_model_requests=false\n'}
        with self.assertRaisesRegex(context.PilotError, 'persistence required'):
            context.validate_overlay({}, arm)
        arm['overlay_toml'] = ''
        with self.assertRaisesRegex(context.PilotError, 'persistence required'):
            context.validate_overlay({}, arm)
        arm['overlay_toml'] = '[observability]\npersist_model_requests=true\n'
        with self.assertRaisesRegex(context.PilotError, 'debug observability required'):
            context.validate_overlay({}, arm)

    def test_context_report_requires_explicit_bounded_tail_metadata_not_snapshot_claim(self):
        import tempfile
        from unittest import mock
        for field, value in [('source', 'future'), ('from_sequence', True),
                             ('events_read', 2), ('pages_read', 0),
                             ('current_tail_reached', False), ('closed_snapshot_verified', True)]:
            report = self.context_report()
            report['history_coverage'][field] = value
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                with mock.patch.object(context, 'json_command', return_value=report):
                    with self.assertRaises(context.PilotError):
                        context.retain_context_observation_report({}, {}, {'arm': 'on'},
                            Path(directory) / 'workspace', 'session', 'turn',
                            self.context_events(), {'complete': True}, 1, 10)

    def test_normalized_context_report_is_consumed_without_raw_receipt_accounting(self):
        import tempfile
        from unittest import mock
        report = self.context_report()
        with tempfile.TemporaryDirectory() as directory:
            result = {}
            with mock.patch.object(context, 'json_command', return_value=report) as public:
                context.retain_context_observation_report(result, {}, {'arm': 'on'},
                    Path(directory) / 'workspace', 'session', 'turn',
                    self.context_events(), {'complete': True}, 1, 10)
            self.assertEqual(public.call_args.args[2][:3],
                             ['session', 'context-observation-report', 'session'])
            self.assertEqual(result['context_observation_report'], report)
            self.assertEqual(result['context_observation_report_receipt']['scope'], 'representation_only')

    @staticmethod
    def context_events():
        return [{'session_id': 'session', 'sequence': 3, 'kind': {'trace_event': {'trace': {
            'turn_id': 'turn', 'payload': {'model_request_built': {}}}}}}]

    @staticmethod
    def context_report():
        return {'schema': 'bcode.eval.context_observations.v1', 'session_id': 'session',
                'scope': 'caller_retained_event_window', 'export_completed': True,
                'events_supplied': 1, 'delivery_claim': False, 'quality_claim': False,
                'window_complete': False,
                'history_coverage': {'source': 'public_session_history', 'from_sequence': 0,
                    'events_read': 1, 'pages_read': 1, 'current_tail_reached': True,
                    'closed_snapshot_verified': False},
                'context_representation_reports': [{'session_id': 'session', 'turn_id': 'turn',
                    'sequence': 5, 'validation': {'status': 'full_receipt_validated',
                        'scope': 'representation_only', 'delivery_claim': False,
                        'quality_claim': False, 'model_request_sequence': 3,
                        'lineage_omitted': 0, 'receipt_omitted': 0,
                        'mandatory': {'count': 1, 'represented': 1, 'dropped': 0, 'unknown': 0},
                        'optional': {'count': 1, 'represented': 1, 'dropped': 0, 'unknown': 0},
                        'optional_sources_certified': True, 'optional_representation': 'all_represented'}}]}

    def test_missing_foreign_future_and_uncertified_reports_fail_closed(self):
        import tempfile
        from unittest import mock
        reports = []
        for field, value in [('schema', 'future'), ('session_id', 'foreign'),
                             ('events_supplied', 0), ('context_representation_reports', [])]:
            report = self.context_report()
            report[field] = value
            reports.append(report)
        for field, value in [('status', 'artifact_not_validated'),
                             ('optional_sources_certified', False),
                             ('model_request_sequence', 5), ('lineage_omitted', 1),
                             ('receipt_omitted', True)]:
            report = self.context_report()
            report['context_representation_reports'][0]['validation'][field] = value
            reports.append(report)
        duplicate = self.context_report()
        duplicate['context_representation_reports'] *= 2
        reports.append(duplicate)
        with tempfile.TemporaryDirectory() as directory:
            for report in reports:
                with self.subTest(report=report), mock.patch.object(context, 'json_command', return_value=report):
                    with self.assertRaises(context.PilotError):
                        context.retain_context_observation_report({}, {}, {'arm': 'on'},
                            Path(directory) / 'workspace', 'session', 'turn',
                            self.context_events(), {'complete': True}, 1, 10)
            with mock.patch.object(context, 'json_command') as public:
                with self.assertRaises(context.PilotError):
                    context.retain_context_observation_report({}, {}, {'arm': 'on'},
                        Path(directory) / 'workspace', 'session', 'turn', [], {'complete': False}, 1, 10)
                public.assert_not_called()

    def test_every_actual_request_requires_an_exact_validated_observation(self):
        import tempfile
        from unittest import mock
        events = self.context_events()
        second = self.context_events()[0]
        second['sequence'] = 4
        events.append(second)
        report = self.context_report()
        report['events_supplied'] = 2
        report['history_coverage']['events_read'] = 2
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(context, 'json_command', return_value=report):
                with self.assertRaises(context.PilotError):
                    context.retain_context_observation_report({}, {}, {'arm': 'on'},
                        Path(directory) / 'workspace', 'session', 'turn',
                        events, {'complete': True}, 1, 10)

    def test_first_public_observation_survives_disposal_and_rejects_foreign_turn(self):
        import tempfile
        from unittest import mock
        spec = importlib.util.spec_from_file_location('retention_normal',
            Path(__file__).with_name('judgement-normal-live.py'))
        normal = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(normal)
        observation = {'schema_version': 1, 'scope': 'normal_turn', 'session_id': 'session', 'turn_id': 'turn',
                       'complete': True, 'omitted': 0, 'terminal': 'completed',
                       'covered_features': ['context_reranking'], 'operations': [
                           {'feature': 'context_reranking', 'trigger': 'disabled',
                            'inference': [], 'inference_complete': True,
                            'application': 'baseline_retained', 'operation_id': 0}]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'observation.json'
            result = {}
            with mock.patch.object(context, 'json_command', side_effect=[observation, None]) as public:
                context.retain_hook_observation(result, path, {}, {'arm': 'off'}, directory,
                                                'session', 'turn', normal)
                original = path.read_bytes()
                context.retain_hook_observation(result, path, {}, {'arm': 'off'}, directory,
                                                'session', 'turn', normal)
                self.assertEqual(public.call_count, 1)
                self.assertEqual(path.read_bytes(), original)
                with self.assertRaises(ValueError):
                    context.retain_hook_observation(result, path, {}, {'arm': 'off'}, directory,
                                                    'session', 'foreign', normal)
            for invalid in (None, {**observation, 'turn_id': 'foreign'}):
                with mock.patch.object(context, 'json_command', return_value=invalid):
                    with self.assertRaises(ValueError):
                        context.retain_hook_observation({}, Path(directory) / 'never.json', {},
                                                        {'arm': 'off'}, directory, 'session', 'turn', normal)
                self.assertFalse((Path(directory) / 'never.json').exists())

    def test_capture_failure_is_not_terminal_or_oracle_failure(self):
        result = {'stage': 'capture', 'capture': {'status': 'public_operation_failed',
                  'outcome': 'infrastructure_failure', 'operation_latency_ms': 1}}
        with self.assertRaises(context.PilotError) as caught:
            context.capture_failure(result)
        context.record_failure(result, caught.exception)
        self.assertEqual(result['failure_stage'], 'capture')
        self.assertEqual(result['capture_failure']['outcome'], 'infrastructure_failure')
        self.assertNotIn('oracle malformed', result['gap'])
        result['capture']['operation_latency_ms'] = 999999
        with self.assertRaises(context.PilotError):
            context.capture_failure(result)
        self.assertEqual(result['capture_failure']['outcome'], 'infrastructure_failure')
        context.capture_failure({'capture': {'status': 'captured', 'outcome': 'succeeded'}})

    def test_imported_driver_classification_and_private_errors(self):
        spec = importlib.util.spec_from_file_location('diagnostic_normal',
            Path(__file__).with_name('judgement-normal-live.py'))
        normal = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(normal)
        error = normal.cli.PilotError('one fresh exact normal turn required')
        result = {'stage': 'terminal'}
        context.record_failure(result, error, normal)
        self.assertEqual(result['failure_stage'], 'terminal')
        self.assertEqual(result['gap'], 'one fresh exact normal turn required')
        for stage in ('capture', 'terminal', 'observation', 'oracle'):
            result = {'stage': stage}
            context.record_failure(result, ValueError('private provider secret'), normal)
            self.assertEqual(result['failure_stage'], stage)
            self.assertNotIn('secret', result['gap'])
            self.assertTrue(result['gap'].startswith(stage))

    def test_report_retains_authored_scope_and_snippet_budget(self):
        job = {'scope': 'independently_authored_synthetic_holdout',
               'source_order': ['old', 'noise', 'current'], 'context_budget_bytes': 132,
               'judgement_selection': {}, 'oracle_sha256': 'oracle', 'scenarios_sha256': 'inputs'}
        report = context.initial_report(job)
        self.assertEqual(report['scope'], job['scope'])
        self.assertEqual(report['snippet_budget_bytes'], 132)
        self.assertEqual(report['budget_scope'], 'optional_snippet_bytes_not_total_delivered_bytes')
        self.assertIsNone(report['framed_installation_bytes'])
        del job['scope']
        self.assertEqual(context.initial_report(job)['scope'], 'development_pilot_not_accuracy_or_promotion')

    def test_arm_order_defaults_and_exact_alternation(self):
        self.assertEqual(context.arm_order({}), ['off', 'on'])
        self.assertEqual(context.arm_order({'arm_order': ['on', 'off']}), ['on', 'off'])
        for value in (None, 'on,off', ['on'], ['off', 'off'], ['on', 'off', 'on']):
            with self.assertRaises(ValueError):
                context.arm_order({'arm_order': value})

    def test_explicit_combined_and_leave_one_out(self):
        self.assertEqual(context.context_treatment({}, {'arm': 'off'}), set())
        self.assertEqual(context.context_treatment({}, {'arm': 'on'}), {'context_reranking'})
        enabled = ['recommendations', 'context_reranking']
        self.assertEqual(context.context_treatment({}, {'arm': 'on', 'enabled_features': enabled}), set(enabled))
        for invalid in [['unknown'], ['recommendations', 'recommendations'], 'context_reranking']:
            with self.assertRaises(ValueError):
                context.context_treatment({}, {'arm': 'on', 'enabled_features': invalid})

    def test_deferred_binding_requires_typed_sentinel_and_verified_artifact(self):
        import copy
        identity = {'artifact_id': 'artifact', 'executable_sha256': 'digest', 'state_id': 'verified-state'}
        job = {'subject_binary': '/immutable/bcode'}
        arm = {'state_binding': 'verified_public_diagnose'}
        template = {'request': {'subject': {**identity, 'executable': job['subject_binary'],
                                           'state_id': 'unknown'}}}
        bound = copy.deepcopy(template)
        context.bind_subject(bound, job, arm, identity)
        self.assertEqual(bound['request']['subject']['state_id'], 'verified-state')
        for invalid in (None, '', 'arbitrary-bound-state', 'verified-state'):
            bad = copy.deepcopy(template)
            bad['request']['subject']['state_id'] = invalid
            before = copy.deepcopy(bad)
            with self.assertRaises(ValueError):
                context.bind_subject(bad, job, arm, identity)
            self.assertEqual(bad, before)
        for key in ('artifact_id', 'executable_sha256', 'executable'):
            bad = copy.deepcopy(template)
            bad['request']['subject'][key] = 'wrong'
            with self.assertRaises(ValueError):
                context.bind_subject(bad, job, arm, identity)
            self.assertEqual(bad['request']['subject']['state_id'], 'unknown')
        for invalid in (None, '', 'unknown'):
            with self.assertRaises(ValueError):
                context.bind_subject(copy.deepcopy(template), job, arm, {**identity, 'state_id': invalid})
        concrete = copy.deepcopy(bound)
        context.bind_subject(concrete, job, {}, identity)
        with self.assertRaises(ValueError):
            context.bind_subject(copy.deepcopy(template), job, {}, identity)

    def test_identity_is_not_delivery_or_mandatory_retention_proof(self):
        proof = context.delivery_proof({'verified': True})
        self.assertEqual(proof['normal_request_identity'], {'verified': True})
        self.assertEqual(proof['solver_content_delivery']['status'], 'unsupported')
        self.assertEqual(proof['mandatory_context_retention']['status'], 'unsupported')


if __name__ == '__main__':
    unittest.main()
