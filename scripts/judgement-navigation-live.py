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

spec = importlib.util.spec_from_file_location('normal_navigation', Path(__file__).with_name('judgement-normal-live.py'))
normal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(normal)
cli = normal.cli


def retained_navigation_observations(job, report):
    """Consume only pinned producer snapshots, never disposable public reads."""
    root = Path(job['run_directory'])
    session, turn = report['session_id'], report['turn_id']
    hook_receipt = report['navigation_hook_receipt']
    source = hook_receipt['source_sequence']
    cli.require(type(source) is int and source >= 0, 'invalid retained source sequence')
    values = {}
    for name in ('hook', 'labels'):
        path = root / ('navigation-' + name + '.json')
        receipt = report['navigation_' + name + '_receipt']
        cli.require(receipt.get('file') == str(path) and not path.is_symlink()
                    and receipt.get('session_id') == session and receipt.get('turn_id') == turn
                    and receipt.get('source_sequence') == source
                    and path.stat().st_size <= cli.MAX_BYTES
                    and cli.digest(path) == receipt.get('sha256'), 'retained navigation integrity mismatch')
        values[name] = cli.read_json(path)
    enabled = 'session_categorization' in report['enabled_features']
    summary = normal.hook_summary(values['hook'], session, turn,
        {'session_categorization': 'applied' if enabled else 'disabled'})
    cli.require(summary['all_required_satisfied'], 'retained navigation hooks incomplete')
    labels = values['labels']
    if enabled:
        cli.require(isinstance(labels, dict) and labels.get('source_sequence') == source
                    and all(isinstance(labels.get(k), str) and 0 < len(labels[k]) <= 128
                            for k in ('task', 'domain')), 'retained navigation labels foreign')
    else:
        cli.require(labels is None, 'disabled retained labels unexpected')
    return source, labels, summary


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
            source, labels, summary = retained_navigation_observations(job, retained)
            card_report.update(source_sequence=source,
                               labels={k: labels[k] for k in ('task', 'domain')} if labels else {},
                               evidence_status='labels_observed' if labels else 'disabled_baseline',
                               hook_evidence=summary,
                               observation_receipts={name: retained['navigation_' + name + '_receipt']
                                                     for name in ('hook', 'labels')})
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


def consumed_disposition(trial, job):
    """Explicit retained evidence; never infer completion from files or re-admit consumed work."""
    disposition = trial.get('disposition')
    if disposition is None:
        return None
    cli.require(isinstance(disposition, dict) and set(disposition) ==
                {'kind', 'report_file', 'report_sha256'}
                and disposition['kind'] in ('retain_completed', 'retain_gap'),
                'unsupported consumed disposition')
    expected = str(Path(job['run_directory']) / 'normal-pilot.json')
    cli.require(trial['adapter'] == 'normal' and disposition['report_file'] == expected
                and cli.digest(expected) == disposition['report_sha256'], 'retained report pin mismatch')
    report = cli.read_json(expected)
    cli.require(report.get('capture_template_sha256') == cli.digest(job['capture_template']),
                'retained template pin mismatch')
    has_gap = report.get('gap') is not None
    cli.require(has_gap == (disposition['kind'] == 'retain_gap'), 'retained outcome mismatch')
    return {'kind': disposition['kind'], 'report_file': expected,
            'report_sha256': disposition['report_sha256'], 'replayed': False}


def preflight_comparison(manifest):
    """Offline validation only; compiled subject decoding is a separate coordinator gate."""
    cli.require(manifest.get('schema_version') == 1 and manifest.get('kind') == 'consumer_comparison',
                'unsupported comparison manifest')
    cli.require(manifest.get('scope') in ('development', 'frozen_study'), 'explicit study scope required')
    trials = manifest['trials']
    cli.require(isinstance(trials, list) and trials and len(trials) <= 1024, 'bounded finite trials required')
    ids, roots, templates = set(), set(), []
    for trial in trials:
        cli.require(trial['id'] not in ids and trial['adapter'] in ('normal', 'context', 'navigation'),
                    'unique supported trial required')
        ids.add(trial['id'])
        cli.require(cli.digest(trial['job_file']) == trial['job_sha256'], 'job pin mismatch')
        job = cli.read_json(trial['job_file'])
        jobs = []
        if trial['adapter'] == 'navigation':
            for card in job['cards']:
                cli.require(cli.digest(card['job_file']) == card['job_sha256'], 'card pin mismatch')
                jobs.append(cli.read_json(card['job_file']))
        else:
            jobs = [job]
        for item in jobs:
            cli.require(cli.digest(item['subject_binary']) == item['subject_sha256'], 'subject pin mismatch')
            cli.require(cli.digest(item['oracle_module']) == item['oracle_sha256'], 'oracle pin mismatch')
            for dependency, expected in item.get('oracle_dependencies', {}).items():
                cli.require(cli.digest(dependency) == expected, 'oracle dependency mismatch')
            if trial['adapter'] == 'context':
                cli.require(cli.digest(Path(item['oracle_module']).with_name('context_scenarios.py')) == item['scenarios_sha256'],
                            'scenario pin mismatch')
                cli.external_path(item['pi_root'])
                cli.require([a['arm'] for a in item['arms']] == ['off', 'on'], 'ordered context pair required')
                off, on = [cli.context_treatment(item, a) for a in item['arms']]
                cli.require('context_reranking' not in off and on == off | {'context_reranking'},
                            'context contrast differs beyond measured feature')
            else:
                starter = cli.read_json(item['starter_files'])
                cli.require(isinstance(starter, dict) and starter, 'normal solver task files required')
                cli.require(type(item['max_history_pages']) is int and 1 <= item['max_history_pages'] <= 32,
                            'bounded normal history required')
            retained = consumed_disposition(trial, item)
            run = cli.external_path(item['run_directory'])
            cli.require(str(run) not in roots and (retained is not None or not any(run.iterdir())),
                        'unique empty or explicitly retained run root required')
            roots.add(str(run))
            arms = item['arms'] if trial['adapter'] == 'context' else [item['arm']]
            for arm in arms:
                state = cli.external_path(arm['state_root'])
                cli.require(str(state) not in roots, 'unique state root required')
                roots.add(str(state))
                sessions = cli.external_path(str(state / 'sessions'))
                cli.require(retained is not None or not any(sessions.iterdir()), 'empty or retained session root required')
                template = cli.read_json(arm.get('capture_template', item.get('capture_template')))
                cli.require(template.get('schema_version') == 1 and template.get('allow_live') is True,
                            'explicit typed live capture template required')
                subject = template['request']['subject']
                cli.require(subject['executable'] == item['subject_binary']
                            and subject['executable_sha256'] == item['subject_sha256']
                            and subject['artifact_id'] == item['artifact_id'], 'template subject mismatch')
                cli.require(template['consumer']['kind'] == 'normal_turn'
                            and template['request']['source'] == 'live_consumer', 'normal consumer required')
                if trial['adapter'] == 'context':
                    cli.verify_capture_isolation(item, arm, template['request'])
                else:
                    normal.treatment_features(item, template, cli.tomllib.loads(arm['overlay_toml'])['judgement'])
                    if item.get('resolve_state_identity') is True:
                        cli.require(template['request']['subject']['state_id'] == 'unknown', 'typed unknown state required')
                templates.append(str(arm.get('capture_template', item.get('capture_template'))))
    return {'schema_version': 1, 'offline_preflight': 'passed', 'templates': templates,
            'compiled_decoder': 'pending_coordinator', 'execution_attempted': False}


def validate_decoder_receipts(manifest, templates):
    """Require pinned coordinator decoder results for every exact template; no inferred decode."""
    receipts = manifest.get('decoder_receipts', [])
    cli.require(isinstance(receipts, list) and len(receipts) == len(templates),
                'all current-subject decoder receipts required')
    seen = set()
    for receipt in receipts:
        template = receipt['template']
        cli.require(template in templates and template not in seen
                    and cli.digest(template) == receipt['template_sha256'], 'decoder template pin mismatch')
        seen.add(template)
        capture = cli.read_json(template)
        cli.require(receipt['subject_sha256'] == capture['request']['subject']['executable_sha256'],
                    'decoder subject mismatch')
        cli.require(cli.digest(receipt['result_file']) == receipt['result_sha256'], 'decoder result pin mismatch')
        decoded = cli.read_json(receipt['result_file'])
        cli.require(decoded == {'schema_version': 1, 'decoded': True, 'execution_attempted': False},
                    'exact compiled decoder result required')


def comparison_evidence(adapter, job):
    """Retain independent outcome and measured overhead, not a selection-derived success flag."""
    if adapter == 'navigation':
        path = job['output_file']
    else:
        path = str(Path(job['run_directory']) / ('pilot.json' if adapter == 'context' else 'normal-pilot.json'))
    report = cli.read_json(path)
    rows = report['arms'] if adapter == 'context' else [report]
    if adapter == 'navigation':
        return {'report_file': path, 'report_sha256': cli.digest(path),
                'native_metric': report.get('native_metric'),
                'baseline_top1_relevant': report.get('baseline_top1_relevant'),
                'adapted_top1_relevant': report.get('adapted_top1_relevant'),
                'optional_telemetry': report.get('optional_telemetry'),
                'gap': report.get('gap'), 'task_outcomes': [c.get('task_truth') for c in report['cards']]}
    return {'report_file': path, 'report_sha256': cli.digest(path), 'arms': [
        {'gap': row.get('gap'), 'independent_oracle': row.get('oracle'),
         'terminal_proof': row.get('terminal_proof'),
         'operation_latency_ms': (row.get('capture') or {}).get('operation_latency_ms'),
         'telemetry': (row.get('capture') or {}).get('telemetry'),
         'delivery_proof': row.get('delivery_proof', cli.delivery_proof(row.get('selection')))} for row in rows]}


def run_comparison(manifest):
    """Serial actual-consumer dispatch; recommendations use normal.run, never catalog scoring."""
    preflight = preflight_comparison(manifest)
    cli.require(manifest.get('decoder_gate') == 'coordinator_verified_current_subject',
                'compiled decoder gate pending')
    validate_decoder_receipts(manifest, preflight['templates'])
    report = {'schema_version': 1, 'scope': manifest['scope'], 'preflight': preflight,
              'trials': [], 'effectiveness': 'pending_independent_terminal_outcomes'}
    output = Path(manifest['output_file'])
    cli.external_path(str(output.parent))
    cli.write_json(output, report)
    for trial in manifest['trials']:
        job = cli.read_json(trial['job_file'])
        result = {'id': trial['id'], 'adapter': trial['adapter'], 'gap': None}
        retained = consumed_disposition(trial, job)
        if retained is not None:
            result['retained'] = retained
            result['evidence'] = comparison_evidence(trial['adapter'], job)
            report['trials'].append(result)
            update_owned_report(output, report)
            if retained['kind'] == 'retain_gap':
                return 1
            continue
        try:
            if trial['adapter'] == 'normal':
                result['exit_code'] = normal.run(job)
            elif trial['adapter'] == 'context':
                result['exit_code'] = cli.run(job)
            else:
                result['exit_code'] = run(job)
            result['evidence'] = comparison_evidence(trial['adapter'], job)
        except (ValueError, KeyError, TypeError, OSError):
            result['gap'] = 'consumer unavailable; effects require coordinator reconciliation'
        report['trials'].append(result)
        update_owned_report(output, report)
        if result.get('gap') or result.get('exit_code', 0) != 0:
            return 1
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--file', required=True, type=Path)
    parser.add_argument('--preflight', action='store_true', help='offline comparison validation; no subject invocation')
    args = parser.parse_args()
    try:
        value = cli.read_json(args.file)
        if args.preflight:
            print(json.dumps(preflight_comparison(value), sort_keys=True))
            return 0
        return run_comparison(value) if value.get('kind') == 'consumer_comparison' else run(value)
    except (ValueError, KeyError, TypeError, OSError):
        parser.exit(1, 'Navigation precondition failed; no automatic replay.\n')


if __name__ == '__main__':
    raise SystemExit(main())
