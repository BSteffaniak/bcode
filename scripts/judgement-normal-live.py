#!/usr/bin/env python3
"""Coordinator-only single normal-path pilot; no daemon startup/build or live calls in tests.

Run: python3 -B scripts/judgement-normal-live.py --file JOB.json
JOB v1 keys: schema_version, subject_binary, subject_sha256, artifact_id, profile,
context (nullable), run_directory (existing empty), arm {state_root, config_file,
overlay_toml}, capture_template (CaptureJob v1), starter_files (JSON filename->text),
scenario_id, expected_selection {provider,model,auth_profile}, oracle_module,
oracle_sha256, oracle_dependencies {absolute_path:sha256}, artifact (relative file),
max_history_pages (1..32). All workspace/state/report paths are external artifacts.
The oracle module exports grade(scenario_id, workspace); its code never enters model
inputs. Freeze task/oracle hashes before execution. Config completeness stays unknown.
The first supported pilot is tool_result_triage; recommendations/eval require actual
public adapters, never a substituted service or scripted producer result.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location('context_cli', Path(__file__).with_name('judgement-context-live.py'))
cli = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cli)

FEATURES = {'tool_result_triage', 'context_reranking', 'progress_monitoring',
            'recommendations', 'session_categorization', 'eval_categorization'}
WRITE_TOOLS = {'filesystem.write', 'filesystem.edit', 'filesystem.multi_edit'}


def sha(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def receipt_projection(event):
    """No arbitrary prose/arguments/results retained; hashes commit the bounded public page."""
    kind = event['kind']
    cli.require(isinstance(kind, dict) and len(kind) == 1, 'unknown canonical event shape')
    name, value = next(iter(kind.items()))
    result = {'sequence': event['sequence'], 'kind': name, 'event_sha256': sha(event)}
    if name == 'model_turn_started' or name == 'model_turn_finished':
        result['turn_id'] = value['turn_id']
        if name == 'model_turn_finished':
            cli.require(value['outcome'] in ('completed', 'failed', 'cancelled', 'provider_unavailable'), 'unknown terminal outcome')
            result['outcome'] = value['outcome']
    elif name in ('positioned_tool_call_requested', 'tool_call_requested'):
        result['tool_call_id_sha256'] = sha(value['tool_call_id'])
        result['filesystem_write_requested'] = value['tool_name'] in WRITE_TOOLS
    elif name == 'tool_invocation_result_recorded':
        # Correlation and success are validated below from canonical typed records.
        record = value['record']
        result['record_sha256'] = sha(record)
        result['invocation_id_sha256'] = sha(record['invocation_id'])
        cli.require(type(record['is_error']) is bool, 'unknown tool result status')
        result['is_error'] = record['is_error']
    return result


def read_history(job, session_id, workspace):
    """Bounded paged canonical API; incomplete/unsupported pages block attribution."""
    events, pages, cursor = [], [], None
    for _ in range(job['max_history_pages']):
        args = ['session', 'history', session_id, '--limit', '100', '--json']
        if cursor is not None:
            args += ['--after', str(cursor)]
        page = cli.json_command(job, job['arm'], args, workspace)
        cli.require(not page.get('compatibility_issues'), 'history compatibility unavailable')
        batch = page['events']
        cli.require(isinstance(batch, list) and len(batch) <= 100, 'history page exceeds bound')
        for event in batch:
            sequence = event['sequence']
            cli.require(event['session_id'] == session_id and type(sequence) is int and sequence >= 0,
                        'foreign or invalid canonical history')
            cli.require(not events or sequence > events[-1]['sequence'], 'history duplicate/backward sequence')
            events.append(event)
        pages.append({'sha256': sha(page), 'count': len(batch), 'has_more': page['has_more']})
        if not page['has_more']:
            return events, {'complete': True, 'pages': pages,
                            'events': [receipt_projection(e) for e in events]}
        next_cursor = (page.get('next_cursor') or {}).get('sequence')
        cli.require(batch and type(next_cursor) is int and next_cursor > batch[-1]['sequence']
                    and (cursor is None or next_cursor > cursor), 'history cursor missing/inconsistent')
        cursor = next_cursor
    raise cli.PilotError('history bound exhausted; task/effect evidence incomplete')


def exact_terminal(events):
    starts = [e for e in events if 'model_turn_started' in e['kind']]
    ends = [e for e in events if 'model_turn_finished' in e['kind']]
    cli.require(len(starts) == len(ends) == 1, 'one fresh exact normal turn required')
    start, end = starts[0], ends[0]
    turn_id = start['kind']['model_turn_started']['turn_id']
    cli.require(end['kind']['model_turn_finished']['turn_id'] == turn_id
                and end['sequence'] > start['sequence'], 'terminal turn correlation mismatch')
    return turn_id, start['sequence'], end['sequence']


def require_available_terminal(events, end_sequence):
    """Known provider absence is an unavailable gap, never successful task evidence."""
    terminal = next(e for e in events if e['sequence'] == end_sequence)
    cli.require(terminal['kind']['model_turn_finished']['outcome'] != 'provider_unavailable',
                'provider unavailable; task evidence unavailable')


def settled_writes(events, start, end):
    """Correlate exact canonical request IDs with later successful result records."""
    # Current positioned requests and the explicitly supported unpositioned public shape
    # share typed tool identity fields; no alternate field-name guessing.
    requests = [(e, value) for e in events if start < e['sequence'] < end
                for name, value in e['kind'].items()
                if name in ('positioned_tool_call_requested', 'tool_call_requested')
                and value.get('tool_name') in WRITE_TOOLS]
    receipts = []
    for request, value in requests:
        invocation = value['tool_call_id']
        matches = [e for e in events if request['sequence'] < e['sequence'] < end
                   and e['kind'].get('tool_invocation_result_recorded', {}).get('record', {}).get('invocation_id') == invocation]
        cli.require(len(matches) == 1, 'missing/ambiguous exact filesystem settlement')
        record = matches[0]['kind']['tool_invocation_result_recorded']['record']
        cli.require(type(record['is_error']) is bool, 'unknown filesystem settlement')
        receipts.append({'request_sequence': request['sequence'], 'result_sequence': matches[0]['sequence'],
                         'invocation_id_sha256': sha(invocation), 'succeeded': not record['is_error']})
    cli.require(receipts and any(r['succeeded'] for r in receipts), 'successful settled filesystem write missing')
    return receipts


def validate_capture(capture, feature, arm, enabled=None, primary_observation=None, control='positive'):
    cli.require(capture['scope'] == 'normal_turn' and capture['request']['feature'] == feature
                and capture['request']['arm'] == arm, 'exact normal consumer identity required')
    telemetry = capture['telemetry']
    cli.require(telemetry['optional_operations_complete'], 'optional operation coverage incomplete')
    cli.require(telemetry['decisions']['omitted_stages'] == 0, 'decision evidence truncated')
    cli.require(control in ('positive', 'negative', 'outage'), 'known control required')
    if control == 'negative':
        cli.require(all(telemetry.get(key) == 0 for key in
                        ('discovery_calls', 'verification_calls', 'inference_calls')),
                    'negative control optional calls not verified zero')
        return
    cli.require(capture['consumer_executed'] and capture.get('output') is not None,
                'normal consumer application unavailable')
    if enabled is not None and primary_observation is not None:
        hook_summary(primary_observation, primary_observation['session_id'], primary_observation['turn_id'], {})
        ops = [op for op in primary_observation['operations'] if op['feature'] == feature]
        cli.require(feature in primary_observation['covered_features'] and ops
                    and all(op['inference_complete'] for op in ops), 'primary hook coverage incomplete')
        if feature not in enabled:
            cli.require(all(op['trigger'] == 'disabled' and not op['inference']
                            and op['application'] == 'baseline_retained' for op in ops),
                        'disabled primary optional work observed')
        else:
            cli.require(any(op['trigger'] == 'triggered' and op['inference']
                            and op['application'] in ('applied', 'baseline_retained') for op in ops),
                        'enabled primary judgement trigger not observed')
    elif arm == 'off':
        cli.require(all(telemetry.get(key) == 0 for key in
                        ('discovery_calls', 'verification_calls', 'inference_calls')), 'off optional calls not verified zero')
    else:
        cli.require(type(telemetry.get('inference_calls')) is int and telemetry['inference_calls'] > 0,
                    'on judgement trigger not observed')


def normalize_capture_template(template):
    """Drop known legacy report metadata; reject other top-level schema drift safely."""
    cli.require(isinstance(template, dict), 'capture template must be an object')
    normalized = dict(template)
    normalized.pop('output_path', None)
    fields = {'schema_version', 'request', 'consumer', 'allow_live'}
    cli.require(set(normalized) == fields,
                'capture template requires only schema_version, request, consumer, allow_live')
    cli.require(type(normalized['schema_version']) is int and normalized['schema_version'] == 1,
                'unsupported capture template schema_version')
    cli.require(type(normalized['allow_live']) is bool,
                'capture template allow_live must be boolean')
    cli.require(isinstance(normalized['request'], dict) and isinstance(normalized['consumer'], dict),
                'capture template request and consumer must be objects')
    return normalized


def preflight_capture_job(job, template, root, workspace):
    """Use the subject's typed decoder without admitting a session or consumer."""
    candidate = json.loads(json.dumps(normalize_capture_template(template)))
    candidate['consumer']['request']['session_id'] = '00000000-0000-4000-8000-000000000001'
    candidate['consumer']['request']['admission']['execution'].update(
        provider_plugin_id=job['expected_selection']['provider'],
        model_id=job['expected_selection']['model'])
    path = root / 'capture-preflight.json'
    cli.write_json(path, candidate)
    receipt = cli.json_command(job, job['arm'],
        ['judgement', 'benchmark-validate', '--file', str(path)], workspace)
    cli.require(receipt == {'schema_version': 1, 'decoded': True, 'execution_attempted': False},
                'capture job typed validation not verified')
    return receipt


def treatment_features(job, template, judgement):
    """Explicit six-feature configurations; primary capture arm describes only its feature."""
    feature = template['request']['feature']
    arm = template['request']['arm']
    mode = job.get('treatment', 'single' if arm == 'on' else 'all_off')
    cli.require(feature in FEATURES and arm in ('off', 'on'), 'known primary feature/arm required')
    sets = {'all_off': set(), 'single': {feature}, 'all_on': set(FEATURES),
            'leave_out': FEATURES - {feature}}
    cli.require(mode in sets, 'unknown treatment configuration')
    enabled = sets[mode]
    cli.require((feature in enabled) == (arm == 'on'), 'primary arm/treatment mismatch')
    cli.require(judgement.get('mode') == 'auto' and
                all(judgement.get(f) == ('auto' if f in enabled else 'off') for f in FEATURES),
                'explicit exact feature overlay required')
    hooks = job.get('required_hooks', {})
    cli.require(isinstance(hooks, dict) and set(hooks) <= FEATURES - {'eval_categorization'} and
                all(v in ('applied', 'baseline_retained', 'disabled') for v in hooks.values()),
                'explicit normal-turn required hooks expected')
    cli.require(all((f in enabled) if v == 'applied' else (f not in enabled) if v == 'disabled' else True
                    for f, v in hooks.items()), 'hook requirement contradicts treatment')
    return mode, enabled


def passive_facts(observation):
    """Decode explicit v2 zero-inference accounting; never reinterpret legacy gaps."""
    version = observation.get('schema_version')
    cli.require(version in (1, 2), 'unsupported turn observation version')
    aggregates = observation.get('passive_aggregates', [])
    accounting = observation.get('accounting')
    if version == 1:
        cli.require(not aggregates and accounting is None, 'legacy aggregate ambiguity')
        return []
    cli.require(isinstance(accounting, dict) and set(accounting) == {'begun', 'omitted_operations'}
                and all(type(v) is int and 0 <= v < 2**64 for v in accounting.values())
                and isinstance(aggregates, list) and len(aggregates) <= 12,
                'invalid aggregate accounting')
    keys = set()
    total = len(observation['operations']) + accounting['omitted_operations']
    for aggregate in aggregates:
        cli.require(set(aggregate) == {'feature', 'trigger', 'application', 'count',
                                       'first_operation_id', 'last_operation_id'}
                    and aggregate['feature'] in FEATURES
                    and aggregate['trigger'] in ('disabled', 'not_applicable')
                    and aggregate['application'] == 'baseline_retained', 'invalid passive qualifier')
        count, first, last = (aggregate[k] for k in ('count', 'first_operation_id', 'last_operation_id'))
        cli.require(all(type(v) is int and 0 <= v < 2**64 for v in (count, first, last))
                    and 0 < count <= last - first + 1 and first <= last < accounting['begun'],
                    'invalid passive ordinals')
        retained = [op['operation_id'] for op in observation['operations']]
        cli.require(first not in retained and last not in retained
                    and (count != 1 or first == last)
                    and count <= last - first + 1 - sum(first <= i <= last for i in retained),
                    'passive ordinal collision')
        key = (aggregate['feature'], aggregate['trigger'])
        cli.require(key not in keys, 'duplicate passive bucket')
        keys.add(key)
        total += count
    endpoints = []
    for aggregate in aggregates:
        endpoints.extend({aggregate['first_operation_id'], aggregate['last_operation_id']})
    cli.require(len(endpoints) == len(set(endpoints)), 'duplicate passive ordinal')
    for first in endpoints:
        for last in endpoints:
            if first > last:
                continue
            required = sum(first <= op['operation_id'] <= last for op in observation['operations'])
            for aggregate in aggregates:
                lo, hi = aggregate['first_operation_id'], aggregate['last_operation_id']
                required += aggregate['count'] if first <= lo and hi <= last else sum(
                    first <= ordinal <= last for ordinal in {lo, hi})
            cli.require(required <= last - first + 1, 'passive ordinal ranges cannot be disjoint')
    ids = [op['operation_id'] for op in observation['operations']]
    cli.require(len(ids) <= 64 and all(type(i) is int and 0 <= i < accounting['begun'] for i in ids)
                and ids == sorted(set(ids)) and total == accounting['begun']
                and accounting['omitted_operations'] <= observation['omitted'], 'operation totals mismatch')
    return aggregates


def hook_summary(observation, session_id, turn_id, required):
    """Validated CLI facts only: independent application checks, never task-success evidence."""
    cli.require(isinstance(observation, dict) and observation.get('scope') == 'normal_turn'
                and observation.get('session_id') == session_id and observation.get('turn_id') == turn_id
                and observation.get('complete') is True and observation.get('omitted') == 0
                and observation.get('terminal') in ('completed', 'failed', 'cancelled'),
                'exact complete normal observation required')
    aggregates = passive_facts(observation)
    operations = observation['operations']
    summary = {}
    for feature, expected in required.items():
        passive = [op for op in aggregates if op['feature'] == feature]
        ops = [op for op in operations if op['feature'] == feature]
        cli.require(feature in observation['covered_features'] and (ops or passive) and
                    all(op['inference_complete'] is True and op['application'] is not None for op in ops),
                    'required hook coverage unavailable')
        if expected == 'disabled':
            satisfied = all(op['trigger'] == 'disabled' and not op['inference']
                            and op['application'] == 'baseline_retained' for op in ops) and all(
                                op['trigger'] == 'disabled' for op in passive)
        else:
            satisfied = any(op['application'] == expected and
                            (expected != 'applied' or op['trigger'] == 'triggered') for op in ops) or (expected == 'baseline_retained' and bool(passive))
        summary[feature] = {'expected': expected, 'satisfied': satisfied,
                            'passive_aggregates': passive,
                            'operations': [{'operation_id': op['operation_id'], 'trigger': op['trigger'],
                                            'application': op['application'],
                                            'inference_count': len(op['inference'])} for op in ops]}
    return {'observation_sha256': sha(observation), 'hooks': summary,
            'all_required_satisfied': all(v['satisfied'] for v in summary.values())}


def retain_navigation_observations(job, report, root, events, session_id, turn_id, workspace):
    """Retain first exact public facts; never refresh an existing receipt."""
    activated = [e for e in events if 'submission_activated' in e['kind']]
    cli.require(len(activated) == 1 and activated[0]['session_id'] == session_id,
                'exact navigation activation unavailable')
    source = activated[0]['sequence']
    cli.require(type(source) is int and source >= 0, 'invalid navigation activation sequence')
    identity = {'session_id': session_id, 'turn_id': turn_id, 'source_sequence': source}
    required = {'session_categorization': 'applied' if 'session_categorization' in report['enabled_features'] else 'disabled'}
    retained = {}
    for name in ('hook', 'labels'):
        path = root / ('navigation-' + name + '.json')
        key = 'navigation_' + name + '_receipt'
        if key in report:
            receipt = report[key]
            cli.require(all(receipt.get(k) == v for k, v in identity.items())
                        and receipt.get('file') == str(path) and not path.is_symlink()
                        and path.stat().st_size <= cli.MAX_BYTES
                        and cli.digest(path) == receipt.get('sha256'), 'navigation receipt integrity mismatch')
            value = cli.read_json(path)
        else:
            value = cli.json_command(job, job['arm'],
                ['judgement', 'observe-turn', session_id, turn_id] if name == 'hook' else
                ['session', 'categorization', session_id, '--json'], workspace)
        if name == 'hook':
            summary = hook_summary(value, session_id, turn_id, required)
            cli.require(summary['all_required_satisfied'], 'navigation hooks incomplete')
            report['hook_evidence'] = summary
        elif 'session_categorization' in report['enabled_features']:
            cli.require(isinstance(value, dict) and value.get('source_sequence') == source
                        and all(isinstance(value.get(k), str) and 0 < len(value[k]) <= 128
                                for k in ('task', 'domain')), 'navigation label identity unavailable')
        else:
            cli.require(value is None, 'disabled navigation labels unexpectedly present')
        cli.require(len(json.dumps(value).encode()) <= cli.MAX_BYTES, 'navigation observation oversized')
        if key not in report:
            cli.write_json(path, value)
            report[key] = dict(identity, file=str(path), sha256=cli.digest(path))
        retained[name] = value
    return retained


def retain_primary_observation(job, report, root, session_id, turn_id, workspace, feature):
    """Always read the closed exact-turn primary ledger, independent of optional hook requests."""
    path = root / 'primary-observation.json'
    receipt = report.get('primary_observation_receipt')
    if receipt is not None:
        cli.require(receipt.get('session_id') == session_id and receipt.get('turn_id') == turn_id
                    and receipt.get('file') == str(path) and not path.is_symlink()
                    and path.stat().st_size <= cli.MAX_BYTES and cli.digest(path) == receipt.get('sha256'),
                    'primary observation receipt mismatch')
        observation = cli.read_json(path)
    else:
        observation = cli.json_command(job, job['arm'],
            ['judgement', 'observe-turn', session_id, turn_id], workspace)
    summary = hook_summary(observation, session_id, turn_id, job.get('required_hooks', {}))
    cli.require(feature in observation['covered_features'], 'primary feature coverage unavailable')
    if receipt is None:
        cli.write_json(path, observation)
        report['primary_observation_receipt'] = {'file': str(path), 'sha256': cli.digest(path),
                                                'session_id': session_id, 'turn_id': turn_id}
    report['hook_evidence'] = summary
    return observation


def observe_hooks(job, session_id, turn_id, workspace):
    observation = cli.json_command(job, job['arm'],
        ['judgement', 'observe-turn', session_id, turn_id], workspace)
    return hook_summary(observation, session_id, turn_id, job.get('required_hooks', {}))


def grade(job, workspace):
    pins = {job['oracle_module']: job['oracle_sha256'], **job['oracle_dependencies']}
    cli.require(all(cli.digest(path) == digest for path, digest in pins.items()), 'oracle source identity changed')
    code = ('import importlib.util,json,sys;from pathlib import Path;'
            'sys.path.insert(0,str(Path(sys.argv[1]).parent));'
            's=importlib.util.spec_from_file_location("independent",sys.argv[1]);'
            'm=importlib.util.module_from_spec(s);s.loader.exec_module(m);'
            'print(json.dumps(m.grade(sys.argv[2],sys.argv[3])))')
    completed = subprocess.run([sys.executable, '-I', '-B', '-c', code, job['oracle_module'],
                                job['scenario_id'], str(workspace)], stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30, check=False)
    cli.require(completed.returncode == 0 and len(completed.stdout) <= 16384, 'independent oracle unavailable')
    result = json.loads(completed.stdout)
    cli.require(result.get('status') in ('pass', 'fail'), 'independent task truth unavailable')
    # Only fixed status plus hashes; arbitrary oracle prose stays outside reports.
    return {'status': result['status'], 'result_sha256': sha(result)}


def bind_normal_subject(job, template, identity):
    """Replace only the explicit typed unknown marker with verified public identity."""
    subject = template['request']['subject']
    if job.get('resolve_state_identity') is True:
        cli.require(subject.get('state_id') == 'unknown',
                    'deferred state requires exact unknown marker; stale identity rejected')
        cli.require(isinstance(identity.get('state_id'), str)
                    and identity['state_id'] not in ('', 'unknown'),
                    'verified public state identity required')
        subject['state_id'] = identity['state_id']
    cli.require(all(subject.get(k) == v for k, v in identity.items())
                and subject['executable'] == job['subject_binary'],
                'declared subject identity mismatch')


def trigger_diagnostic(events, session_id, feature, observation=None, oracle=None):
    """Bounded public facts, not inferred benefit or inferred absence of effects."""
    cli.require(len(events) <= 3200, 'diagnostic history budget exceeded')
    scoped = [e for e in events if e.get('session_id') == session_id]
    starts = [e for e in scoped if 'model_turn_started' in e['kind']]
    terminals = [e for e in scoped if 'model_turn_finished' in e['kind']]
    exact = len(starts) == 1
    turn = starts[0]['kind']['model_turn_started']['turn_id'] if exact else None
    finished = [e for e in terminals if exact and
                e['kind']['model_turn_finished']['turn_id'] == turn]
    start = starts[0]['sequence'] if exact else None
    end = finished[0]['sequence'] if len(finished) == 1 else None
    if end is not None and end <= start:
        finished, end = [], None
    requests = [(e, value) for e in scoped for name, value in e['kind'].items()
                if name in ('positioned_tool_call_requested', 'tool_call_requested')
                and exact and e['sequence'] > start and (end is None or e['sequence'] < end)]
    results = [e for e in scoped if 'tool_invocation_result_recorded' in e['kind']]
    pairs, errors = 0, 0
    for request, value in requests:
        matches = [e for e in results if e['sequence'] > request['sequence']
                   and (end is None or e['sequence'] < end)
                   and e['kind']['tool_invocation_result_recorded']['record'].get('invocation_id')
                       == value.get('tool_call_id')]
        if len(matches) == 1:
            status = matches[0]['kind']['tool_invocation_result_recorded']['record'].get('is_error')
            if type(status) is bool:
                pairs += 1
                errors += int(status)
    ops = [op for op in (observation or {}).get('operations', []) if op.get('feature') == feature]
    identity_matches = bool(exact and observation and observation.get('session_id') == session_id
                            and observation.get('turn_id') == turn)
    if not identity_matches:
        ops = []
    triggered = sum(op.get('trigger') == 'triggered' for op in ops)
    applied = sum(op.get('application') == 'applied' for op in ops)
    evidence_complete = identity_matches and (observation or {}).get('complete') is True and (observation or {}).get('omitted') == 0
    if not exact:
        stage = 'admission_unknown'
    elif len(finished) != 1:
        stage = 'settlement_unknown'
    elif not requests:
        stage = 'no_observed_tool_call_eligibility_unknown'
    elif not evidence_complete:
        stage = 'observation_unavailable_or_incomplete'
    elif not ops:
        stage = 'feature_evidence_empty'
    elif not triggered:
        stage = 'trigger_not_observed'
    elif not applied:
        stage = 'observed_not_applied'
    else:
        stage = 'applied_quality_gain_unproven'
    return {'stage': stage, 'history_coverage': 'bounded_public_history',
            'admission_verified': exact, 'terminal_verified': len(finished) == 1,
            'turn_id': turn, 'tool_requests': len(requests), 'exact_tool_settlements': pairs,
            'tool_error_settlements': errors, 'tool_success_settlements': pairs - errors,
            'unsettled_or_ambiguous_requests': len(requests) - pairs,
            'feature_operations': len(ops), 'triggered_operations': triggered,
            'applied_operations': applied, 'observation_complete': evidence_complete,
            'task_truth': (oracle or {}).get('status'), 'quality_gain_claimed': False}


def diagnostic_tool_contract(execution, required):
    """An explicit ordinary-tool fixture checks selection, never bypasses authorization."""
    cli.require(isinstance(required, list) and len(required) <= 16
                and all(isinstance(tool, str) and tool for tool in required),
                'bounded required diagnostic tools expected')
    allowlist = execution.get('tool_allowlist')
    excluded = [tool for tool in required if allowlist and tool not in allowlist]
    cli.require(not excluded, 'required ordinary diagnostic tool excluded by admission allowlist')
    return {'required_tool_count': len(required), 'allowlist_checked': bool(allowlist),
            'authorization': 'normal_policy_required', 'availability': 'not_verified_by_prompt'}


def run(job):
    cli.require(job['schema_version'] == 1, 'unsupported normal pilot schema')
    root = cli.external_path(job['run_directory'])
    cli.require(not any(root.iterdir()), 'fresh empty report directory required')
    cli.external_path(job['arm']['state_root'])
    cli.external_path(str(Path(job['arm']['state_root']) / 'sessions'))
    cli.external_path(str(cli.EXTERNAL / 'tmp'))
    cli.require(type(job['max_history_pages']) is int and 1 <= job['max_history_pages'] <= 32,
                'bounded history allowance required')
    template = normalize_capture_template(cli.read_json(job['capture_template']))
    overlay = cli.tomllib.loads(job['arm']['overlay_toml'])
    judgement = overlay['judgement']
    arm = template['request']['arm']
    feature = template['request']['feature']
    treatment, enabled = treatment_features(job, template, judgement)
    selection = judgement['selection']
    cli.require(selection['provider_plugin_id'] == 'bcode.jev'
                and selection['model_id'].startswith('jev-') and selection['auth_profile'],
                'explicit versioned Jev pin required')
    cli.require(feature in FEATURES and template['allow_live']
                and template['consumer']['kind'] == 'normal_turn'
                and template['request']['source'] == 'live_consumer', 'explicit normal live consumer job required')
    cli.require(feature != 'eval_categorization',
                'eval categorization needs coordinator actual eval adapter; do not spend')
    execution = template['consumer']['request']['admission']['execution']
    cli.require(execution.get('permission_mode', 'enforce') == 'enforce', 'normal permissions required')
    cli.require(not execution.get('historical_context'), 'context pilot must use dedicated canonical-import driver')
    pins = {job['oracle_module']: job['oracle_sha256'], **job['oracle_dependencies']}
    cli.require(all(Path(path).is_absolute() and cli.digest(path) == digest for path, digest in pins.items()),
                'independent oracle identity mismatch')
    starter = cli.read_json(job['starter_files'])
    cli.require(isinstance(starter, dict) and 0 < len(starter) <= 32, 'bounded authored task files required')
    workspace = root / 'workspace'
    workspace.mkdir()
    for name, text in starter.items():
        path = Path(name)
        cli.require(not path.is_absolute() and '..' not in path.parts and isinstance(text, str)
                    and len(text.encode()) <= 65536, 'unsafe/unbounded task input')
        target = workspace / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding='utf-8')
    artifact = Path(job['artifact'])
    cli.require(not artifact.is_absolute() and '..' not in artifact.parts, 'confined artifact required')
    report = {'schema_version': 1, 'scope': 'actual_normal_consumer_plus_independent_artifact',
              'feature': feature, 'treatment': treatment, 'enabled_features': sorted(enabled),
              'configuration_complete': False, 'gap': None,
              'capture_template_sha256': cli.digest(job['capture_template']),
              'starter_files_sha256': cli.digest(job['starter_files']), 'oracle_pins': pins,
              'history_before': None, 'history_after': None, 'capture': None, 'oracle': None}
    session_id = None
    events, primary_observation = [], None
    report['capture_stage'] = 'diagnostic_tool_contract'
    try:
        required_tools = job.get('required_diagnostic_tools',
                                 ['shell.run'] if feature == 'tool_result_triage' else [])
        report['diagnostic_tool_contract'] = diagnostic_tool_contract(execution, required_tools)
        report['capture_stage'] = 'subject_verification'
        identity = cli.verify_subject(job, job['arm'], workspace)
        bind_normal_subject(job, template, identity)
        report['capture_stage'] = 'capture_schema_validation'
        report['capture_preflight'] = preflight_capture_job(job, template, root, workspace)
        report['capture_stage'] = 'session_creation'
        created = cli.json_command(job, job['arm'], ['session', 'create', '--cwd', str(workspace), '--json'], workspace)
        session_id = str(uuid.UUID(created['id']))
        report['session_id'] = session_id
        report['capture_stage'] = 'selection_verification'
        report['selection'] = cli.verify_selection(job, job['arm'], workspace, session_id)
        before, report['history_before'] = read_history(job, session_id, workspace)
        cli.require(not any('model_turn_started' in e['kind'] or 'user_message' in e['kind'] for e in before),
                    'new session not fresh')
        template['consumer']['request']['session_id'] = session_id
        execution.update(provider_plugin_id=job['expected_selection']['provider'], model_id=job['expected_selection']['model'])
        template['request']['subject']['configuration_complete'] = False
        job_path = root / 'capture-job.json'
        cli.write_json(job_path, template)
        report['capture_job_sha256'] = cli.digest(job_path)
        report['capture_stage'] = 'normal_turn_capture'
        raw = cli.command(job, job['arm'], ['judgement', 'benchmark-capture', '--file', str(job_path)], workspace, timeout=330)
        report['capture'] = json.loads(raw)
        # Persist completed capture BEFORE any downstream history/oracle read can fail.
        cli.write_json(root / 'capture-result.json', report['capture'])
        report['capture_stage'] = 'public_history'
        events, report['history_after'] = read_history(job, session_id, workspace)
        cli.require([sha(e) for e in events[:len(before)]] == [sha(e) for e in before], 'canonical prefix changed')
        turn_id, start, end = exact_terminal(events)
        report['turn_id'] = turn_id
        require_available_terminal(events, end)
        report['capture_stage'] = 'hook_observation'
        primary_observation = retain_primary_observation(
            job, report, root, session_id, turn_id, workspace, feature)
        if feature == 'session_categorization':
            navigation = retain_navigation_observations(
                job, report, root, events, session_id, turn_id, workspace)['hook']
            cli.require(navigation == primary_observation, 'conflicting closed primary observation')
        report['capture_stage'] = 'independent_oracle'
        report['oracle'] = grade(job, workspace)
        report['capture_stage'] = 'capture_validation'
        validate_capture(report['capture'], feature, template['request']['arm'],
                         enabled, primary_observation,
                         job.get('control', 'positive'))
        if job.get('control', 'positive') != 'negative':
            report['filesystem_write_receipts'] = settled_writes(events, start, end)
            report['settled_write_receipt_verified'] = True
        else:
            report['filesystem_write_receipts'] = []
            report['settled_write_receipt_verified'] = False  # no mutation required by negative control
        path = workspace / artifact
        cli.require(not path.is_symlink(), 'unsafe artifact')
        report['artifact_sha256'] = cli.digest(path) if path.is_file() else None
        cli.require(any('tool_invocation_result_recorded' in e['kind'] for e in events
                        if start < e['sequence'] < end), 'settled tool result receipts absent')
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        report['gap'] = str(error) if isinstance(error, cli.PilotError) else 'public evidence/oracle unavailable'
        report['effects_reconciled'] = False
        if session_id is not None and not events:
            try:
                events, report['history_after'] = read_history(job, session_id, workspace)
                report['gap_history_observation'] = 'bounded_public_read'
            except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired):
                report['gap_history_observation'] = 'unavailable'
        if events and primary_observation is None:
            try:
                observed_turn, _, _ = exact_terminal(events)
                primary_observation = cli.json_command(job, job['arm'],
                    ['judgement', 'observe-turn', session_id, observed_turn], workspace)
                report['gap_hook_observation'] = 'bounded_public_read'
            except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired):
                report['gap_hook_observation'] = 'unavailable'
    report['trigger_diagnostic'] = trigger_diagnostic(
        events, session_id, feature, primary_observation, report.get('oracle'))
    cli.write_json(root / 'normal-pilot.json', report)
    return 1 if report['gap'] else 0


def reconcile(job, report_path, output_path):
    """Read-only subject operations: never replay admission or execute model work."""
    cli.external_path(str(report_path))
    cli.external_path(str(output_path.parent))
    cli.require(not output_path.exists(), 'fresh reconciliation output required')
    retained = cli.read_json(report_path)
    cli.require(retained.get('capture') is not None and retained.get('session_id'),
                'retained completed capture/session required; no automatic rerun')
    root = cli.external_path(job['run_directory'])
    workspace = root / 'workspace'
    cli.require(cli.digest(job['subject_binary']) == job['subject_sha256'], 'subject hash changed')
    report = dict(retained)
    if 'trigger_diagnostic' in retained:
        report['prior_trigger_diagnostic'] = retained['trigger_diagnostic']
    events, observation = [], None
    report.pop('hook_evidence', None)
    report.update(gap=None, reconciliation_source_sha256=cli.digest(report_path),
                  admission_replayed=False)
    try:
        session_id = str(uuid.UUID(retained['session_id']))
        events, report['history_after'] = read_history(job, session_id, workspace)
        retained_before = (retained.get('history_before') or {}).get('events', [])
        cli.require(retained_before and
                    [e['event_sha256'] for e in report['history_after']['events'][:len(retained_before)]]
                    == [e['event_sha256'] for e in retained_before], 'retained canonical prefix mismatch')
        turn_id, start, end = exact_terminal(events)
        report['turn_id'] = turn_id
        require_available_terminal(events, end)
        capture = retained['capture']
        current_observation = retain_primary_observation(
            job, report, root, session_id, turn_id, workspace, capture['request']['feature'])
        observation = current_observation
        subject = capture['request']['subject']
        cli.require(subject['executable_sha256'] == job['subject_sha256']
                    and subject['executable'] == job['subject_binary'], 'retained capture subject mismatch')
        validate_capture(capture, capture['request']['feature'], capture['request']['arm'])
        report['filesystem_write_receipts'] = settled_writes(events, start, end)
        report['settled_write_receipt_verified'] = True
        report['oracle'] = grade(job, workspace)
        artifact = Path(job['artifact'])
        cli.require(not artifact.is_absolute() and '..' not in artifact.parts, 'confined artifact required')
        path = workspace / artifact
        cli.require(not path.is_symlink(), 'unsafe artifact')
        report['artifact_sha256'] = cli.digest(path) if path.is_file() else None
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        report['gap'] = str(error) if isinstance(error, cli.PilotError) else 'reconciliation evidence unavailable'
    report['trigger_diagnostic'] = trigger_diagnostic(
        events, retained['session_id'], retained['capture']['request']['feature'],
        observation, report.get('oracle'))
    cli.write_json(output_path, report)
    return 1 if report['gap'] else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--file', type=Path, required=True)
    parser.add_argument('--reconcile-report', type=Path)
    parser.add_argument('--reconcile-output', type=Path)
    args = parser.parse_args()
    if bool(args.reconcile_report) != bool(args.reconcile_output):
        parser.error('reconciliation requires both --reconcile-report and --reconcile-output')
    try:
        job = cli.read_json(args.file)
        if args.reconcile_report:
            return reconcile(job, args.reconcile_report, args.reconcile_output)
        return run(job)
    except cli.PilotError as error:
        parser.exit(1, str(error) + '\n')
    except KeyError as error:
        # Key name only: do not display job contents, config values or exception payloads.
        missing = str(error.args[0]) if error.args else 'unknown'
        allowed = set(('schema_version subject_binary subject_sha256 artifact_id profile run_directory arm '
                       'capture_template starter_files scenario_id expected_selection oracle_module oracle_sha256 '
                       'oracle_dependencies artifact max_history_pages state_root config_file overlay_toml').split())
        parser.exit(1, 'Missing required normal-pilot field: ' + (missing if missing in allowed else 'nested contract field') + '\n')
    except (ValueError, TypeError, OSError):
        parser.exit(1, 'Normal pilot input malformed/unavailable; no admission replay.\n')


if __name__ == '__main__':
    raise SystemExit(main())
