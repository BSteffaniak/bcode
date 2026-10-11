"""Offline native-navigation adapter tests; mocked CLI is not live product evidence."""
import copy
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('navigation', Path(__file__).with_name('judgement-navigation-live.py'))
nav = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nav)


def cards():
    return [{'id': 'card-' + str(i), 'session_id': '00000000-0000-0000-0000-' + str(i + 1).zfill(12),
             'source_sequence': 9, 'labels': labels, 'title': 'MUST NOT LEAK', 'task_truth': 'pass'}
            for i, labels in enumerate(({}, {'task': 'documentation', 'domain': 'backend'},
                                        {'task': 'documentation', 'domain': 'tooling'}))]


class NavigationTests(unittest.TestCase):
    def test_independent_native_metric_uses_actual_order_not_query_labels(self):
        metric = nav.native_metric(['s1', 's2', 's3'], ['a', 'b', 'c'], {'c'})
        self.assertEqual(metric['status'], 'fail')
        self.assertEqual(metric['reciprocal_rank'], 1 / 3)
        self.assertEqual(metric['ordered_session_ids'], ['s1', 's2', 's3'])
        self.assertTrue(nav.native_metric(['s3', 's1', 's2'], ['c', 'a', 'b'], {'c'})['top1_relevant'])

    def test_aggregate_observed_counts_and_public_hooks_fail_closed(self):
        observed = cards()
        for index, card in enumerate(observed):
            card['optional_telemetry'] = {'optional_operations_complete': True,
                'discovery_calls': index, 'verification_calls': index + 1, 'inference_calls': 2}
            card['hook_evidence'] = {'all_required_satisfied': True, 'observation_sha256': str(index)}
        telemetry, hooks = nav.aggregate_evidence(observed)
        self.assertEqual(telemetry['discovery_calls'], 3)
        self.assertEqual(telemetry['verification_calls'], 6)
        self.assertEqual(telemetry['inference_calls'], 6)
        self.assertEqual(len(hooks['cards']), 3)
        for missing in (None, False, -1):
            bad = copy.deepcopy(observed)
            bad[0]['optional_telemetry']['inference_calls'] = missing
            with self.assertRaises(ValueError):
                nav.aggregate_evidence(bad)
        observed[0]['hook_evidence']['all_required_satisfied'] = False
        with self.assertRaises(ValueError):
            nav.aggregate_evidence(observed)

    def test_title_free_exact_public_label_request_and_no_label_baseline(self):
        query = {'task': 'documentation', 'domain': 'tooling'}
        request = nav.rank_request(cards(), query)
        self.assertEqual(set(request), {'version', 'task', 'domain', 'candidates'})
        self.assertEqual(request['candidates'][2]['categorization'],
                         {'source_sequence': 9, 'task': 'documentation', 'domain': 'tooling'})
        self.assertNotIn('MUST NOT LEAK', str(request))
        self.assertNotIn('task_truth', str(request))
        baseline = nav.rank_request(cards(), query, baseline=True)
        self.assertTrue(all(c['categorization'] is None for c in baseline['candidates']))
        ids = [c['session_id'] for c in baseline['candidates']]
        self.assertEqual(nav.validate_rank_response(baseline, {'version': 1, 'ordered_session_ids': ids,
                         'matched_session_ids': [], 'applied': False}), ids)

    def test_response_exact_permutation_matches_and_applied_fail_closed(self):
        request = nav.rank_request(cards(), {'task': 'documentation', 'domain': 'tooling'})
        ids = [c['session_id'] for c in request['candidates']]
        response = {'version': 1, 'ordered_session_ids': [ids[2], ids[0], ids[1]],
                    'matched_session_ids': [ids[2]], 'applied': True}
        self.assertEqual(nav.validate_rank_response(request, response), response['ordered_session_ids'])
        for key, value in (('version', True), ('version', 2), ('applied', 1), ('applied', False),
                           ('ordered_session_ids', [ids[2], ids[2], ids[0]]),
                           ('matched_session_ids', [ids[1], ids[2]]),
                           ('ordered_session_ids', [ids[2], ids[1], ids[0]])):
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                nav.validate_rank_response(request, dict(response, **{key: value}))
        with self.assertRaises(ValueError):
            nav.validate_rank_response(request, dict(response, extra='unknown'))
        stale = copy.deepcopy(request)
        stale['candidates'][2]['categorization']['source_sequence'] = 8
        with self.assertRaises(ValueError):
            nav.validate_rank_response(stale, response)

    def test_actual_cli_command_path_and_context_are_preserved(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('BCODE_EVAL_TMP', '/Volumes/ehdd/bcode-eval-artifacts/tmp')) as root:
            job = {'run_directory': root, 'arm': {'state_root': '/exact/state', 'config_file': '/exact/config'}}
            request_path = Path(root) / 'rank.json'
            ids = [c['session_id'] for c in cards()]
            reply = {'version': 1, 'ordered_session_ids': ids, 'matched_session_ids': [], 'applied': False}
            with patch.object(nav.cli, 'json_command', return_value=reply) as command:
                ordered, actual, digest = nav.product_rank(job, cards(), {'task': 'documentation'}, request_path, baseline=True)
            command.assert_called_once_with(job, job['arm'],
                ['session', 'rank-categories', '--file', str(request_path)], Path(root) / 'workspace')
            self.assertEqual(ordered, ids)
            self.assertEqual(actual, reply)
            self.assertEqual(digest, nav.cli.digest(request_path))

    def test_invalid_current_sequence_or_duplicate_uuid_rejects(self):
        original = cards()
        original[1]['source_sequence'] = True
        with self.assertRaises(ValueError):
            nav.rank_request(original, {'domain': 'backend'})
        original = cards()
        original[1]['session_id'] = original[0]['session_id']
        with self.assertRaises(ValueError):
            nav.rank_request(original, {'domain': 'backend'})


if __name__ == '__main__':
    unittest.main()
