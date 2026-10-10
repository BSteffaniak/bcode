#!/usr/bin/env python3
"""Offline behavior tests; never invoke a subject, daemon, provider, or oracle."""
import importlib.util
from pathlib import Path
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('normal_pilot', Path(__file__).with_name('judgement-normal-live.py'))
pilot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pilot)


def event(sequence, kind):
    return {'sequence': sequence, 'session_id': 's', 'kind': kind}


class NormalPilotTests(unittest.TestCase):
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
        observation = {'scope': 'normal_turn', 'session_id': 's', 'turn_id': 't', 'complete': True,
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

    def test_navigation_uses_labels_not_relevance_or_request_text(self):
        spec = importlib.util.spec_from_file_location('navigation', Path(__file__).with_name('judgement-navigation-live.py'))
        navigation = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(navigation)
        cards = [{'id': 'a', 'labels': {}}, {'id': 'b', 'labels': {'task': 'fix'}},
                 {'id': 'c', 'labels': {'task': 'fix'}}]
        self.assertEqual([c['id'] for c in navigation.rank_cards(cards, {'task': 'fix'})], ['b', 'c', 'a'])
        self.assertEqual([c['id'] for c in navigation.rank_cards(cards, {'domain': 'unknown'})], ['a', 'b', 'c'])

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
