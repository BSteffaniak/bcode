#!/usr/bin/env python3
"""Coordinator-only live navigation study: --file MANIFEST.json.

Manifest v1: cards [{id, job_file, job_sha256}] (three in chronological display
order), query_labels {task, domain}, relevant_card_ids (independent original-
request hand labels), output_file. Card jobs use normal session_categorization
capture and independent artifact oracles. Product session rank-categories receives
only exact public label/sequence observations; no titles or relevance labels.
Both no-label baseline and observed-label arms use that same native operation.
Runs serially; a failed/retained card is never automatically retried.
"""
import argparse
import importlib.util
import json
import os
import tempfile
from pathlib import Path
import time

spec = importlib.util.spec_from_file_location('normal_navigation', Path(__file__).with_name('judgement-normal-live.py'))
normal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(normal)
cli = normal.cli


def update_owned_report(output, report):
    """Atomically replace only the report exclusively created by this invocation."""
    cli.require(output.is_file() and not output.is_symlink(), 'owned report unavailable')
    with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=output.parent,
                                     prefix='.navigation-', delete=False) as stream:
        temporary = Path(stream.name)
        json.dump(report, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, output)


def validate_relevance(ids, relevant):
    cli.require(relevant and relevant < set(ids), 'nonempty proper relevance subset required')


def rank_request(cards, query, baseline=False):
    """Title/relevance-free input from observed public labels, never oracle labels."""
    import uuid
    candidates = []
    for card in cards:
        session = card['session_id']
        cli.require(str(uuid.UUID(session)) == session, 'canonical session UUID required')
        sequence = card['source_sequence']
        cli.require(type(sequence) is int and sequence > 0, 'current activation required')
        labels = card.get('labels')
        category = None if baseline or not labels else dict(labels, source_sequence=sequence)
        if category is not None:
            cli.require(set(category) == {'source_sequence', 'task', 'domain'}, 'unexpected label fields')
        candidates.append({'session_id': session, 'source_sequence': sequence, 'categorization': category})
    cli.require(len({c['session_id'] for c in candidates}) == len(candidates), 'duplicate session UUID')
    return {'version': 1, 'task': query.get('task'), 'domain': query.get('domain'), 'candidates': candidates}


def validate_rank_response(request, response):
    """Validate product output; never substitute a harness-generated ranking."""
    cli.require(isinstance(response, dict) and set(response) == {
        'version', 'ordered_session_ids', 'matched_session_ids', 'applied'}, 'unknown ranking response fields')
    cli.require(type(response['version']) is int and response['version'] == 1
                and type(response['applied']) is bool, 'invalid ranking response version/flag')
    original = [c['session_id'] for c in request['candidates']]
    ordered, matched = response['ordered_session_ids'], response['matched_session_ids']
    cli.require(isinstance(ordered, list) and all(isinstance(i, str) for i in ordered)
                and len(ordered) == len(original) and set(ordered) == set(original), 'ranking is not exact permutation')
    cli.require(isinstance(matched, list) and all(isinstance(i, str) for i in matched)
                and len(set(matched)) == len(matched) and set(matched) <= set(original), 'invalid matched identities')
    eligible = []
    for candidate in request['candidates']:
        labels = candidate['categorization']
        if (labels is not None and labels['source_sequence'] == candidate['source_sequence']
                and any(request[k] is not None for k in ('task', 'domain'))
                and all(request[k] is None or request[k] != 'other' and labels[k] == request[k]
                        for k in ('task', 'domain'))):
            eligible.append(candidate['session_id'])
    cli.require(matched == eligible, 'product matches do not correspond to public labels')
    cli.require(ordered == matched + [i for i in original if i not in set(matched)], 'product ranking is not stable')
    cli.require(response['applied'] == (ordered != original), 'ranking applied flag mismatch')
    return ordered


def product_rank(job, cards, query, request_path, baseline=False):
    """Actual public product operation; runtime context preserved by existing CLI helper."""
    request = rank_request(cards, query, baseline)
    cli.write_json(request_path, request)
    workspace = Path(job['run_directory']) / 'workspace'
    response = cli.json_command(job, job['arm'],
        ['session', 'rank-categories', '--file', str(request_path)], workspace)
    ordered = validate_rank_response(request, response)
    return ordered, response, cli.digest(request_path)


def run(manifest):
    cli.require(manifest['schema_version'] == 1 and len(manifest['cards']) == 3,
                'exact three-card manifest required')
    cards = manifest['cards']
    ids = [card['id'] for card in cards]
    cli.require(len(set(ids)) == 3 and all(isinstance(i, str) and i for i in ids), 'unique card IDs required')
    query = manifest['query_labels']
    cli.require(isinstance(query, dict) and query and set(query) <= {'task', 'domain'}
                and all(isinstance(v, str) and v and len(v) <= 128 for v in query.values()),
                'bounded explicit query labels required')
    relevant = set(manifest['relevant_card_ids'])
    validate_relevance(ids, relevant)
    output = Path(manifest['output_file'])
    cli.external_path(str(output.parent))
    cli.require(not output.exists(), 'fresh navigation report required')
    jobs = []
    # Validate every immutable input before first admission.
    for card in cards:
        cli.require(cli.digest(card['job_file']) == card['job_sha256'], 'card job identity changed')
        job = cli.read_json(card['job_file'])
        template = cli.read_json(job['capture_template'])
        cli.require(template['request']['feature'] == 'session_categorization', 'actual categorization job required')
        root = cli.external_path(job['run_directory'])
        cli.require(not any(root.iterdir()), 'fresh card run required; reconcile existing work instead')
        jobs.append(job)
    cli.require(len({str(Path(job['arm']['state_root']).resolve()) for job in jobs}) == 3,
                'three independently isolated card states required')
    report = {'schema_version': 1, 'scope': 'actual_session_navigation', 'manifest_sha256': normal.sha(manifest),
              'cards': [], 'gap': None, 'chronological_order': ids}
    cli.write_json(output, report)  # exclusive initial creation; subsequent writes update this owned file
    try:
        for card, job in zip(cards, jobs):
            status = normal.run(job)
            retained_path = Path(job['run_directory']) / 'normal-pilot.json'
            retained = cli.read_json(retained_path)
            card_report = {'id': card['id'], 'session_id': retained.get('session_id'),
                           'source_sequence': None, 'labels': {},
                           'task_truth': (retained.get('oracle') or {}).get('status'),
                           'producer_gap': retained.get('gap'),
                           'producer_report_sha256': cli.digest(retained_path),
                           'evidence_status': 'producer_gap' if status else 'settled'}
            report['cards'].append(card_report)
            update_owned_report(output, report)
            cli.require(status == 0, 'card execution/evidence gap; no retry')
            session = retained['session_id']
            workspace = Path(job['run_directory']) / 'workspace'
            events, _ = normal.read_history(job, session, workspace)
            activated = [e for e in events if 'submission_activated' in e['kind']]
            cli.require(len(activated) == 1, 'exact activation source unavailable')
            source = activated[0]['sequence']
            enabled = 'session_categorization' in retained['enabled_features']
            labels = None
            if enabled:
                for _ in range(5):
                    labels = cli.json_command(job, job['arm'], ['session', 'categorization', session, '--json'], workspace)
                    if isinstance(labels, dict) and labels.get('source_sequence') == source:
                        break
                    time.sleep(0.1)
                cli.require(isinstance(labels, dict) and labels.get('source_sequence') == source
                            and all(isinstance(labels.get(k), str) and 0 < len(labels[k]) <= 128
                                    for k in ('task', 'domain')), 'exact activation labels unavailable')
            card_report.update(source_sequence=source,
                               labels={k: labels[k] for k in ('task', 'domain')} if labels else {},
                               evidence_status='labels_observed' if labels else 'disabled_baseline')
            turn_id, _, _ = normal.exact_terminal(events)
            observation = cli.json_command(job, job['arm'],
                ['judgement', 'observe-turn', session, turn_id], workspace)
            card_report['hook_evidence'] = normal.hook_summary(observation, session, turn_id,
                {'session_categorization': 'applied' if enabled else 'disabled'})
            card_report['optional_telemetry'] = retained['capture']['telemetry']
            update_owned_report(output, report)
        baseline_ids, baseline_response, baseline_hash = product_rank(
            jobs[0], report['cards'], query, output.with_name(output.stem + '-baseline-request.json'), baseline=True)
        ranked_ids, rank_response, rank_hash = product_rank(
            jobs[0], report['cards'], query, output.with_name(output.stem + '-rank-request.json'))
        by_session = {card['session_id']: card['id'] for card in report['cards']}
        baseline_order = [by_session[session] for session in baseline_ids]
        ranked = [by_session[session] for session in ranked_ids]
        cli.require(baseline_order == ids, 'product no-label baseline changed chronology')
        telemetry, hooks = aggregate_evidence(report['cards'])
        report.update(native_metric=native_metric(ranked_ids, ranked, relevant),
                      optional_telemetry=telemetry, hook_evidence=hooks,
                      capture={'scope': 'actual_session_navigation', 'status': 'captured',
                               'telemetry': telemetry})
        report.update(product_rank_response=rank_response, product_baseline_response=baseline_response,
                      rank_request_sha256=rank_hash, baseline_request_sha256=baseline_hash,
                      product_operation='session rank-categories --file',
                      ranked_order=ranked, baseline_top1_relevant=baseline_order[0] in relevant,
                      adapted_top1_relevant=ranked[0] in relevant,
                      benefit=baseline_order[0] not in relevant and ranked[0] in relevant)
    except (ValueError, KeyError, TypeError, OSError, normal.subprocess.TimeoutExpired) as error:
        report['gap'] = str(error) if isinstance(error, cli.PilotError) else 'navigation evidence unavailable'
    update_owned_report(output, report)
    return 1 if report['gap'] else 0


def native_metric(ordered_sessions, ordered_cards, relevant):
    """Independent case oracle; relevance is never part of the product request."""
    validate_relevance(ordered_cards, set(relevant))
    first = next(index for index, card in enumerate(ordered_cards, 1) if card in relevant)
    return {'scope': 'product_native', 'status': 'pass' if first == 1 else 'fail',
            'ordered_session_ids': ordered_sessions, 'ordered_card_ids': ordered_cards,
            'relevant_card_ids': sorted(relevant), 'top1_relevant': first == 1,
            'reciprocal_rank': 1 / first}


def aggregate_evidence(cards):
    """Sum observed counters only; absence is unknown rather than zero."""
    cli.require(len(cards) == 3, 'complete three-card evidence required')
    telemetry = {'optional_operations_complete': True}
    for key in ('discovery_calls', 'verification_calls', 'inference_calls'):
        values = [card['optional_telemetry'].get(key) for card in cards]
        cli.require(all(type(value) is int and value >= 0 for value in values),
                    'unknown card optional counter')
        telemetry[key] = sum(values)
    cli.require(all(card['optional_telemetry'].get('optional_operations_complete') is True
                    for card in cards), 'incomplete card optional evidence')
    hooks = [{'id': card['id'], 'session_id': card['session_id'],
              'source_sequence': card['source_sequence'], 'labels': card['labels'],
              'evidence': card['hook_evidence']} for card in cards]
    cli.require(all(hook['evidence']['all_required_satisfied'] is True for hook in hooks),
                'card public label stage inconsistent')
    return telemetry, {'cards': hooks, 'observation_sha256': normal.sha(hooks),
                       'all_required_satisfied': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--file', required=True, type=Path)
    args = parser.parse_args()
    try:
        return run(cli.read_json(args.file))
    except (ValueError, KeyError, TypeError, OSError):
        parser.exit(1, 'Navigation precondition failed; no automatic replay.\n')


if __name__ == '__main__':
    raise SystemExit(main())
