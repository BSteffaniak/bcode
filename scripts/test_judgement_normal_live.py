#!/usr/bin/env python3
"""Offline behavior tests; never invoke a subject, daemon, provider, or oracle."""
import importlib.util
from pathlib import Path
import tempfile
import json
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('normal_pilot', Path(__file__).with_name('judgement-normal-live.py'))
pilot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pilot)


def event(sequence, kind):
    return {'sequence': sequence, 'session_id': 's', 'kind': kind}


def capture_job_fixture():
    """CaptureJob v1 input, including the typed normal-turn admission envelope."""
    return {
        'schema_version': 1, 'allow_live': True,
        'request': {
            'preregistration_id': 'synthetic-pair', 'task_id': 'synthetic-task',
            'repetition': 0, 'feature': 'context_reranking', 'arm': 'off',
            'source': 'live_consumer', 'capture_id': 'synthetic-off',
            'pairing_id': 'synthetic-pair', 'isolated_feature': True,
            'subject': {
                'executable': '/synthetic/bcode', 'executable_sha256': '0' * 64,
                'artifact_id': 'synthetic-artifact', 'configuration_sha256': 'unknown',
                'configuration_complete': False, 'provider': 'synthetic-provider',
                'model': 'synthetic-model', 'auth_profile': 'synthetic-account',
                'model_profile': 'synthetic-profile', 'state_id': 'synthetic-state',
            },
            'cache_condition': 'unknown', 'timeout_ms': 240000,
        },
        'consumer': {'kind': 'normal_turn', 'request': {
            'session_id': '00000000-0000-4000-8000-000000000001',
            'prompt': 'Implement a pure numeric function in solution.py.',
            'admission': {'execution': {
                'schema_version': 8, 'permission_mode': 'enforce',
                'allow_user_questions': False, 'agent_profile': 'build',
                'provider_plugin_id': 'synthetic-provider', 'model_id': 'synthetic-model',
                'tool_allowlist': ['filesystem.read', 'filesystem.write'],
            }},
            'max_events': 1200, 'categorization_dimension': 'task',
        }},
    }


class NormalPilotTests(unittest.TestCase):
    def test_no_hook_option_routes_primary_trigger_through_closed_ledger_not_total_calls(self):
        observation = {'schema_version': 1, 'scope': 'normal_turn', 'session_id': 's',
                       'turn_id': 't', 'complete': True, 'omitted': 0, 'terminal': 'completed',
                       'covered_features': ['tool_result_triage'], 'operations': [{
                           'feature': 'tool_result_triage', 'operation_id': 0,
                           'trigger': 'triggered', 'inference_complete': True,
                           'inference': [{}], 'application': 'applied'}]}
        capture = {'scope': 'normal_turn', 'request': {'feature': 'tool_result_triage', 'arm': 'on'},
                   'consumer_executed': True, 'output': {'kind': 'classification', 'label': None},
                   'telemetry': {'optional_operations_complete': True,
                                 'decisions': {'omitted_stages': 0}, 'inference_calls': 2}}
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(pilot.cli, 'json_command', return_value=observation):
                retained = pilot.retain_primary_observation({'arm': {}}, {}, Path(directory),
                    's', 't', Path(directory), 'tool_result_triage')
            pilot.validate_capture(capture, 'tool_result_triage', 'on',
                                   {'tool_result_triage'}, retained)
            retained['operations'][0]['trigger'] = 'not_triggered'
            with self.assertRaises(pilot.cli.PilotError):
                pilot.validate_capture(capture, 'tool_result_triage', 'on',
                                       {'tool_result_triage'}, retained)

    def test_primary_observation_is_fetched_without_required_hooks_and_retained(self):
        observation = {'schema_version': 1, 'scope': 'normal_turn', 'session_id': 's',
                       'turn_id': 't', 'complete': True, 'omitted': 0, 'terminal': 'completed',
                       'covered_features': ['tool_result_triage'], 'operations': []}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = {}
            with mock.patch.object(pilot.cli, 'json_command', return_value=observation) as public:
                retained = pilot.retain_primary_observation({'arm': {}}, report, root,
                    's', 't', root, 'tool_result_triage')
                self.assertEqual(retained, observation)
                self.assertEqual(public.call_args.args[2], ['judgement', 'observe-turn', 's', 't'])
            with mock.patch.object(pilot.cli, 'json_command') as public:
                self.assertEqual(pilot.retain_primary_observation({'arm': {}}, report, root,
                    's', 't', root, 'tool_result_triage'), observation)
                public.assert_not_called()
            with self.assertRaises(pilot.cli.PilotError):
                pilot.retain_primary_observation({'arm': {}}, report, root,
                    's', 'foreign', root, 'tool_result_triage')

    def test_capture_template_strips_only_legacy_metadata_and_rejects_drift(self):
        fixture = capture_job_fixture()
        legacy = dict(fixture, output_path='private/legacy-output')
        self.assertEqual(pilot.normalize_capture_template(legacy), fixture)
        self.assertIn('output_path', legacy)
        for invalid in [dict(fixture, private_secret_field='secret'),
                        {key: value for key, value in fixture.items() if key != 'consumer'}]:
            with self.assertRaisesRegex(ValueError, 'requires only') as error:
                pilot.normalize_capture_template(invalid)
            self.assertNotIn('private_secret_field', str(error.exception))
            self.assertNotIn('secret', str(error.exception))

    def test_capture_preflight_writes_normalized_typed_job_and_never_admits(self):
        fixture = capture_job_fixture()
        fixture['consumer']['request']['session_id'] = 'unbound'
        job = {'arm': {}, 'expected_selection': {
            'provider': 'synthetic-provider', 'model': 'synthetic-model'}}
        receipt = {'schema_version': 1, 'decoded': True, 'execution_attempted': False}
        with tempfile.TemporaryDirectory() as directory:
            def validate(_job, _arm, arguments, workspace):
                self.assertEqual(arguments[:3], ['judgement', 'benchmark-validate', '--file'])
                candidate = json.loads(Path(arguments[3]).read_text())
                expected = capture_job_fixture()
                self.assertEqual(candidate, expected)
                return receipt
            with mock.patch.object(pilot.cli, 'json_command', side_effect=validate) as command:
                result = pilot.preflight_capture_job(job, fixture, Path(directory), directory)
                self.assertEqual(result, receipt)
                command.assert_called_once()
            self.assertEqual(fixture['consumer']['request']['session_id'], 'unbound')
            rejected = Path(directory) / 'rejected'
            rejected.mkdir()
            with mock.patch.object(pilot.cli, 'json_command', return_value=dict(receipt, execution_attempted=True)):
                with self.assertRaisesRegex(ValueError, 'not verified'):
                    pilot.preflight_capture_job(job, fixture, rejected, directory)

    def test_reconcile_replaces_stale_diagnostic_with_exact_public_observation(self):
        session = '00000000-0000-4000-8000-000000000001'
        events = [event(1, {'model_turn_started': {'turn_id': 't'}}),
                  event(2, {'tool_call_requested': {'tool_call_id': 'c', 'tool_name': 'shell.run'}}),
                  event(3, {'tool_invocation_result_recorded': {'record': {'invocation_id': 'c', 'is_error': True}}}),
                  event(4, {'model_turn_finished': {'turn_id': 't', 'outcome': 'completed'}})]
        for item in events:
            item['session_id'] = session
        for arm in ('off', 'on', 'foreign', 'incomplete', 'missing'):
            with self.subTest(arm=arm), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'workspace').mkdir()
                job = {'run_directory': directory, 'subject_binary': '/synthetic/bcode',
                       'subject_sha256': 'hash', 'arm': {}, 'artifact': 'solution.py'}
                capture = capture_job_fixture()
                capture['request']['feature'] = 'tool_result_triage'
                capture['request']['subject'].update(executable='/synthetic/bcode', executable_sha256='hash')
                prior = {'stage': 'admission_unknown'}
                retained = {'capture': capture, 'session_id': session,
                            'history_before': {'events': [{'event_sha256': 'prefix'}]},
                            'trigger_diagnostic': prior}
                observation = {'schema_version': 1, 'scope': 'normal_turn', 'session_id': session, 'turn_id': 't',
                               'terminal': 'completed', 'complete': True, 'omitted': 0,
                               'operations': [{'feature': 'tool_result_triage',
                                               'trigger': 'triggered' if arm == 'on' else 'disabled',
                                               'application': 'applied' if arm == 'on' else 'baseline_retained'}],
                               'aggregates': [], 'covered_features': ['tool_result_triage']}
                if arm == 'foreign':
                    observation['turn_id'] = 'foreign'
                if arm == 'incomplete':
                    observation['complete'] = False
                from contextlib import ExitStack
                with ExitStack() as stack:
                    for name, value in [('external_path', lambda path: Path(path)),
                                        ('read_json', lambda path: retained),
                                        ('digest', lambda path: 'hash')]:
                        stack.enter_context(mock.patch.object(pilot.cli, name, side_effect=value))
                    command = stack.enter_context(mock.patch.object(pilot.cli, 'json_command',
                        **({'side_effect': ValueError('unavailable')} if arm == 'missing' else {'return_value': observation})))
                    stack.enter_context(mock.patch.object(pilot, 'read_history', return_value=(events, {'events': [{'event_sha256': 'prefix'}]})))
                    stack.enter_context(mock.patch.object(pilot, 'validate_capture'))
                    stack.enter_context(mock.patch.object(pilot, 'settled_writes', return_value=[]))
                    stack.enter_context(mock.patch.object(pilot, 'grade', return_value={'status': 'pass'}))
                    output = root / 'reconciled.json'
                    status = pilot.reconcile(job, root / 'report.json', output)
                    result = json.loads(output.read_text())
                    self.assertEqual(result['prior_trigger_diagnostic'], prior)
                    diagnostic = result['trigger_diagnostic']
                    self.assertTrue(diagnostic['admission_verified'])
                    self.assertTrue(diagnostic['terminal_verified'])
                    self.assertEqual(diagnostic['stage'],
                        'applied_quality_gain_unproven' if arm == 'on' else
                        'trigger_not_observed' if arm == 'off' else 'observation_unavailable_or_incomplete')
                    self.assertEqual(status, 0 if arm in ('off', 'on') else 1)
                    self.assertFalse(result['admission_replayed'])
                    command.assert_called_once_with(job, job['arm'],
                        ['judgement', 'observe-turn', session, 't'], root / 'workspace')

    def test_natural_trigger_gap_stages_do_not_claim_quality_gain(self):
        started = event(1, {'model_turn_started': {'turn_id': 't'}})
        finished = event(4, {'model_turn_finished': {'turn_id': 't', 'outcome': 'completed'}})
        requested = event(2, {'tool_call_requested': {'tool_call_id': 'c', 'tool_name': 'shell.run'}})
        result = event(3, {'tool_invocation_result_recorded': {'record': {'invocation_id': 'c', 'is_error': True}}})
        feature = 'tool_result_triage'
        diagnostic = lambda events, observation=None: pilot.trigger_diagnostic(events, 's', feature, observation, {'status': 'pass'})
        self.assertEqual(diagnostic([])['stage'], 'admission_unknown')
        self.assertEqual(diagnostic([started])['stage'], 'settlement_unknown')
        self.assertEqual(diagnostic([started, finished])['stage'], 'no_observed_tool_call_eligibility_unknown')
        events = [started, requested, result, finished]
        observation = {'session_id': 's', 'turn_id': 't', 'complete': True, 'omitted': 0, 'operations': []}
        self.assertEqual(diagnostic(events)['stage'], 'observation_unavailable_or_incomplete')
        self.assertEqual(diagnostic(events, observation)['stage'], 'feature_evidence_empty')
        observation['operations'] = [{'feature': feature, 'trigger': 'triggered', 'application': 'baseline_retained'}]
        self.assertEqual(diagnostic(events, observation)['stage'], 'observed_not_applied')
        observation['operations'][0]['application'] = 'applied'
        value = diagnostic(events, observation)
        self.assertEqual(value['stage'], 'applied_quality_gain_unproven')
        self.assertEqual(value['exact_tool_settlements'], 1)
        self.assertEqual(value['tool_error_settlements'], 1)
        self.assertFalse(value['quality_gain_claimed'])
        observation['turn_id'] = 'foreign'
        self.assertEqual(diagnostic(events, observation)['stage'], 'observation_unavailable_or_incomplete')

    def test_diagnostic_fixture_checks_allowlist_without_authorization_bypass(self):
        with self.assertRaises(pilot.cli.PilotError):
            pilot.diagnostic_tool_contract({'tool_allowlist': ['filesystem.read']}, ['shell.run'])
        value = pilot.diagnostic_tool_contract({'tool_allowlist': ['shell.run']}, ['shell.run'])
        self.assertEqual(value['authorization'], 'normal_policy_required')
        self.assertEqual(value['availability'], 'not_verified_by_prompt')

    def test_deferred_subject_requires_typed_unknown_and_exact_verified_identity(self):
        identity = {'artifact_id': 'artifact', 'executable_sha256': 'digest', 'state_id': 'verified'}
        job = {'resolve_state_identity': True, 'subject_binary': '/subject'}
        def template(state='unknown'):
            return {'request': {'subject': dict(identity, executable='/subject', state_id=state)}}
        value = template()
        pilot.bind_normal_subject(job, value, identity)
        self.assertEqual(value['request']['subject']['state_id'], 'verified')
        for marker in (None, '', 'stale', 'UNRESOLVED_DO_NOT_INVOKE', 'verified'):
            with self.assertRaisesRegex(ValueError, 'unknown marker'):
                pilot.bind_normal_subject(job, template(marker), identity)
        missing = template()
        del missing['request']['subject']['state_id']
        with self.assertRaisesRegex(ValueError, 'unknown marker'):
            pilot.bind_normal_subject(job, missing, identity)
        for key in ('artifact_id', 'executable_sha256', 'executable'):
            bad = template()
            bad['request']['subject'][key] = 'wrong'
            with self.assertRaisesRegex(ValueError, 'identity mismatch'):
                pilot.bind_normal_subject(job, bad, identity)
        with self.assertRaisesRegex(ValueError, 'verified public'):
            pilot.bind_normal_subject(job, template(), dict(identity, state_id='unknown'))
        pilot.bind_normal_subject(dict(job, resolve_state_identity=False), template('verified'), identity)
        with self.assertRaisesRegex(ValueError, 'identity mismatch'):
            pilot.bind_normal_subject(dict(job, resolve_state_identity=False), template(), identity)

    def test_zero_origin_bounded_pages_and_content_free_receipts(self):
        first = event(0, {'session_created': {'working_directory': '/external'}})
        last = event(1, {'assistant_message': {'text': 'secret-output'}})
        pages = [{'events': [first], 'has_more': True, 'next_cursor': {'sequence': 1}},
                 {'events': [last], 'has_more': False, 'next_cursor': None}]
        with mock.patch.object(pilot.cli, 'json_command', side_effect=pages) as command:
            events, projection = pilot.read_history({'arm': {}, 'max_history_pages': 2}, 's', '/workspace')
        self.assertEqual(events, [first, last])
        self.assertTrue(projection['complete'])
        self.assertNotIn('secret-output', str(projection))
        self.assertEqual(command.call_args_list[1].args[2][-2:], ['--after', '1'])

    def test_cursor_is_first_unreturned_not_last_returned_or_guessed_plus_one(self):
        pages = [{'events': [event(0, {'session_created': {}})], 'has_more': True,
                  'next_cursor': {'sequence': 7}},
                 {'events': [event(7, {'assistant_message': {'text': 'bounded'}})], 'has_more': False}]
        with mock.patch.object(pilot.cli, 'json_command', side_effect=pages) as command:
            events, _ = pilot.read_history({'arm': {}, 'max_history_pages': 2}, 's', '/workspace')
        self.assertEqual([e['sequence'] for e in events], [0, 7])
        self.assertEqual(command.call_args_list[1].args[2][-2:], ['--after', '7'])

    def test_missing_pages_wrong_session_and_duplicate_sequence_fail(self):
        for pages in ([{'events': [event(0, {'session_created': {}})], 'has_more': True}],
                      [{'events': [dict(event(0, {'session_created': {}}), session_id='foreign')], 'has_more': False}],
                      [{'events': [event(0, {'session_created': {}}), event(0, {'session_created': {}})], 'has_more': False}],
                      [{'events': [], 'has_more': False, 'compatibility_issues': ['future']} ]):
            with self.subTest(pages=pages), mock.patch.object(pilot.cli, 'json_command', side_effect=pages), \
                    self.assertRaises(pilot.cli.PilotError):
                pilot.read_history({'arm': {}, 'max_history_pages': 1}, 's', '/workspace')

    def test_provider_unavailable_is_retained_but_never_task_success(self):
        events = [event(1, {'model_turn_started': {'turn_id': 't'}}),
                  event(5, {'model_turn_finished': {'turn_id': 't', 'outcome': 'provider_unavailable'}})]
        with mock.patch.object(pilot.cli, 'json_command', return_value={
                'events': events, 'has_more': False, 'compatibility_issues': []}):
            actual, projection = pilot.read_history({'arm': {}, 'max_history_pages': 1}, 's', '/workspace')
        self.assertEqual(projection['events'][-1]['outcome'], 'provider_unavailable')
        self.assertEqual(pilot.exact_terminal(actual), ('t', 1, 5))
        with self.assertRaisesRegex(pilot.cli.PilotError, 'provider unavailable; task evidence unavailable'):
            pilot.require_available_terminal(actual, 5)
        events[-1]['kind']['model_turn_finished']['outcome'] = 'future_unknown'
        with self.assertRaisesRegex(pilot.cli.PilotError, 'unknown terminal outcome'):
            pilot.receipt_projection(events[-1])

    def test_v2_passive_counts_and_legacy_compatibility(self):
        feature = 'progress_monitoring'
        observation = {'schema_version': 2, 'scope': 'normal_turn', 'session_id': 's', 'turn_id': 't',
                       'complete': True, 'omitted': 0, 'terminal': 'completed', 'covered_features': [feature],
                       'operations': [], 'accounting': {'begun': 200, 'omitted_operations': 0},
                       'passive_aggregates': [{'feature': feature, 'trigger': 'disabled',
                           'application': 'baseline_retained', 'count': 200,
                           'first_operation_id': 0, 'last_operation_id': 199}]}
        summary = pilot.hook_summary(observation, 's', 't', {feature: 'disabled'})
        self.assertTrue(summary['all_required_satisfied'])
        self.assertEqual(summary['hooks'][feature]['passive_aggregates'][0]['count'], 200)
        self.assertEqual(summary['hooks'][feature]['operations'], [])
        import copy
        interleaved = copy.deepcopy(observation)
        interleaved['accounting']['begun'] = 201
        interleaved['operations'] = [{'operation_id': 100, 'feature': feature, 'trigger': 'disabled',
                                    'application': 'baseline_retained', 'inference': [],
                                    'inference_complete': True}]
        interleaved['passive_aggregates'][0]['last_operation_id'] = 200
        self.assertTrue(pilot.hook_summary(interleaved, 's', 't', {feature: 'disabled'})['all_required_satisfied'])
        collision = copy.deepcopy(interleaved)
        collision['operations'][0]['operation_id'] = 200
        with self.assertRaises(pilot.cli.PilotError):
            pilot.passive_facts(collision)
        collision = copy.deepcopy(observation)
        collision['passive_aggregates'].append(dict(collision['passive_aggregates'][0], feature='tool_result_triage'))
        collision['accounting']['begun'] = 400
        with self.assertRaises(pilot.cli.PilotError):
            pilot.passive_facts(collision)
        for mutation in ('count', 'active', 'future', 'legacy', 'omitted'):
            invalid = copy.deepcopy(observation)
            if mutation == 'count': invalid['accounting']['begun'] += 1
            elif mutation == 'active': invalid['passive_aggregates'][0]['trigger'] = 'triggered'
            elif mutation == 'future': invalid['schema_version'] = 3
            elif mutation == 'legacy': invalid['schema_version'] = 1
            else: invalid['omitted'] = 1
            with self.subTest(mutation=mutation), self.assertRaises(pilot.cli.PilotError):
                pilot.hook_summary(invalid, 's', 't', {feature: 'disabled'})

    def test_only_exact_turn_terminal_is_truth(self):
        events = [event(1, {'model_turn_started': {'turn_id': 't'}}),
                  event(8, {'model_turn_finished': {'turn_id': 't', 'outcome': 'failed'}})]
        self.assertEqual(pilot.exact_terminal(events), ('t', 1, 8))
        events[-1]['kind']['model_turn_finished']['turn_id'] = 'other'
        with self.assertRaises(pilot.cli.PilotError):
            pilot.exact_terminal(events)

    def test_write_request_needs_exact_later_successful_receipt(self):
        requested = event(2, {'tool_call_requested': {'tool_name': 'filesystem.write', 'tool_call_id': 'i'}})
        recorded = event(3, {'tool_invocation_result_recorded': {'record': {'invocation_id': 'i', 'is_error': False,
                                                                          'model_output': 'private'}}})
        receipts = pilot.settled_writes([requested, recorded], 1, 4)
        self.assertTrue(receipts[0]['succeeded'])
        self.assertNotIn('private', str(receipts))
        for events in ([requested], [requested, recorded, dict(recorded, sequence=4)]):
            with self.assertRaises(pilot.cli.PilotError):
                pilot.settled_writes(events, 1, 5)
        recorded['kind']['tool_invocation_result_recorded']['record']['is_error'] = True
        with self.assertRaises(pilot.cli.PilotError):
            pilot.settled_writes([requested, recorded], 1, 4)

    def test_current_positioned_write_receipt_and_content_free_projection(self):
        requested = event(2, {'positioned_tool_call_requested': {
            'turn_id': 'turn-1', 'output_position': 0, 'tool_call_id': 'call_exact',
            'producer_plugin_id': 'bcode.filesystem', 'tool_name': 'filesystem.write',
            'arguments_json': '{"path":"artifact","contents":"private"}',
            'working_directory': '/authorized/workspace'}})
        recorded = event(3, {'tool_invocation_result_recorded': {'record': {
            'invocation_id': 'call_exact', 'is_error': False, 'model_output': 'private',
            'presentation': None, 'result': {'written': True}}}})
        receipts = pilot.settled_writes([requested, recorded], 1, 4)
        self.assertEqual(receipts[0]['invocation_id_sha256'], pilot.sha('call_exact'))
        projection = pilot.receipt_projection(requested)
        self.assertTrue(projection['filesystem_write_requested'])
        self.assertEqual(projection['tool_call_id_sha256'], receipts[0]['invocation_id_sha256'])
        self.assertNotIn('private', str(projection))
        recorded['kind']['tool_invocation_result_recorded']['record']['invocation_id'] = 'other'
        with self.assertRaises(pilot.cli.PilotError):
            pilot.settled_writes([requested, recorded], 1, 4)

    def test_explicit_treatments_and_independent_hook_evidence(self):
        feature = 'tool_result_triage'
        for treatment, arm, enabled in (
                ('all_off', 'off', set()), ('single', 'on', {feature}),
                ('all_on', 'on', set(pilot.FEATURES)),
                ('leave_out', 'off', pilot.FEATURES - {feature})):
            template = {'request': {'feature': feature, 'arm': arm}}
            overlay = {'mode': 'auto', **{f: 'auto' if f in enabled else 'off' for f in pilot.FEATURES}}
            self.assertEqual(pilot.treatment_features({'treatment': treatment}, template, overlay)[1], enabled)
            overlay[feature] = 'auto' if arm == 'off' else 'off'
            with self.assertRaises(pilot.cli.PilotError):
                pilot.treatment_features({'treatment': treatment}, template, overlay)
        observation = {'schema_version': 1, 'scope': 'normal_turn', 'session_id': 's', 'turn_id': 't', 'complete': True,
                       'omitted': 0, 'terminal': 'completed', 'covered_features': [feature],
                       'operations': [{'feature': feature, 'operation_id': 0, 'trigger': 'triggered',
                                       'inference_complete': True, 'inference': [{}], 'application': 'baseline_retained'}]}
        result = pilot.hook_summary(observation, 's', 't', {feature: 'applied'})
        self.assertFalse(result['all_required_satisfied'])  # absence is not task failure or fabricated application
        observation['operations'][0]['application'] = 'applied'
        self.assertTrue(pilot.hook_summary(observation, 's', 't', {feature: 'applied'})['all_required_satisfied'])
        with self.assertRaises(pilot.cli.PilotError):
            pilot.hook_summary(observation, 's', 'wrong', {})
        observation['operations'][0].update(trigger='disabled', inference=[], application='baseline_retained')
        capture = {'scope': 'normal_turn', 'request': {'feature': feature, 'arm': 'off'},
                   'consumer_executed': True, 'output': {'kind': 'classification', 'label': None},
                   'telemetry': {'optional_operations_complete': True,
                                 'decisions': {'omitted_stages': 0}, 'inference_calls': 3}}
        # Leave-out permits other enabled hooks' calls, never calls by this disabled hook.
        pilot.validate_capture(capture, feature, 'off', pilot.FEATURES - {feature}, observation)
        observation['operations'][0]['inference'] = [{}]
        with self.assertRaises(pilot.cli.PilotError):
            pilot.validate_capture(capture, feature, 'off', pilot.FEATURES - {feature}, observation)

    def test_navigation_partial_reports_update_and_relevance_is_discriminating(self):
        import tempfile
        spec = importlib.util.spec_from_file_location('navigation_updates', Path(__file__).with_name('judgement-navigation-live.py'))
        navigation = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(navigation)
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / 'report.json'
            navigation.cli.write_json(output, {'cards': []})
            navigation.update_owned_report(output, {'cards': [{'id': 'a'}], 'gap': None})
            navigation.update_owned_report(output, {'cards': [{'id': 'a'}, {'id': 'b'}], 'gap': 'evidence gap'})
            self.assertEqual(len(navigation.cli.read_json(output)['cards']), 2)
            self.assertEqual(navigation.cli.read_json(output)['gap'], 'evidence gap')
        navigation.validate_relevance(['a', 'b', 'c'], {'b'})
        for relevant in (set(), {'a', 'b', 'c'}, {'foreign'}):
            with self.assertRaises(navigation.cli.PilotError):
                navigation.validate_relevance(['a', 'b', 'c'], relevant)

    def test_negative_no_work_requires_verified_zero_without_fake_application(self):
        capture = {'scope': 'normal_turn', 'request': {'feature': 'tool_result_triage', 'arm': 'on'},
                   'consumer_executed': False, 'output': None,
                   'telemetry': {'optional_operations_complete': True,
                                 'decisions': {'omitted_stages': 0}, 'discovery_calls': 0,
                                 'verification_calls': 0, 'inference_calls': 0}}
        pilot.validate_capture(capture, 'tool_result_triage', 'on', control='negative')
        for value in (None, 1):
            capture['telemetry']['inference_calls'] = value
            with self.assertRaises(pilot.cli.PilotError):
                pilot.validate_capture(capture, 'tool_result_triage', 'on', control='negative')

    def test_failed_task_terminal_is_not_infrastructure_failure(self):
        capture = {'scope': 'normal_turn', 'request': {'feature': 'tool_result_triage', 'arm': 'off'},
                   'consumer_executed': True, 'output': {'kind': 'classification', 'label': None},
                   'outcome': 'failed', 'telemetry': {'optional_operations_complete': True,
                   'decisions': {'omitted_stages': 0}, 'discovery_calls': 0, 'verification_calls': 0, 'inference_calls': 0}}
        pilot.validate_capture(capture, 'tool_result_triage', 'off')
        capture['telemetry']['inference_calls'] = None
        with self.assertRaises(pilot.cli.PilotError):
            pilot.validate_capture(capture, 'tool_result_triage', 'off')


if __name__ == '__main__':
    unittest.main()
