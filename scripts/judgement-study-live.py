#!/usr/bin/env python3
"""Finite, serialized actual-consumer study; coordinator owns all live execution.

materialize --file PLAN.json: schema_version=1, corpus_file/corpus_sha256,
base_job, subject {executable,sha256,artifact_id}, output_root, oracle_module,
oracle_dependencies [paths], and optional bindings {trial_id:{adapter,job_file,
job_sha256,report_file,metric}}. Allowed adapters: normal/context/navigation/eval.
Bindings are exact authored actual-adapter jobs, not substitutions. Trial IDs:
CASE/repetition-N/on|off. combined_trials uses the same explicit binding contract.
run --file SCHEDULE --limit N: finite serial admissions; pre-admission receipt
fences ambiguous work. Existing receipts require explicit reconciliation, never retry.
reconcile --file SCHEDULE: reads retained reports only, no CLI or provider calls.
reduce --file SCHEDULE: coding truth, native navigation/label metrics, gaps and
latency separately; missing trials are never failures or zero-call observations.
"""
import argparse
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location('normal_study_driver', Path(__file__).with_name('judgement-normal-live.py'))
normal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(normal)
cli = normal.cli
ADAPTERS = {'normal', 'context', 'navigation', 'eval'}
FROZEN_HASH = '799edca329ab14bca76b69bbb9856badd641394abf8b542f545681c995918421'


def atomic(path, value):
    # External invocation-owned result only. Never rewrite corpus/schedule inputs.
    import os
    import tempfile
    with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=path.parent, delete=False) as stream:
        temporary = stream.name
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def pinned_copy(path, root):
    path = Path(path)
    digest = cli.digest(path)
    destination = root / path.name
    cli.require(not destination.exists(), 'oracle snapshot name collision')
    shutil.copyfile(path, destination)
    cli.require(cli.digest(destination) == digest, 'oracle snapshot mismatch')
    return str(destination), digest


def materialize(plan):
    cli.require(plan['schema_version'] == 1, 'unsupported study plan')
    corpus_path = Path(plan['corpus_file'])
    cli.require(cli.digest(corpus_path) == plan['corpus_sha256'], 'corpus pin mismatch')
    corpus = cli.read_json(corpus_path)
    if corpus['split'] == 'new-heldout':
        cli.require(plan['corpus_sha256'] == FROZEN_HASH, 'heldout bytes changed')
    cases = {c['case_id']: c for c in corpus['cases']}
    cli.require(len(cases) == len(corpus['cases']), 'duplicate corpus cases')
    root = cli.external_path(plan['output_root'])
    cli.require(not any(root.iterdir()), 'fresh study output root required')
    base = cli.read_json(plan['base_job'])
    template_base = cli.read_json(base['capture_template'])
    subject = plan['subject']
    cli.require(cli.digest(subject['executable']) == subject['sha256'], 'subject binary mismatch')
    snapshots = root / 'oracle-snapshot'
    snapshots.mkdir()
    oracle, oracle_hash = pinned_copy(plan['oracle_module'], snapshots)
    dependencies = dict(pinned_copy(p, snapshots) for p in plan['oracle_dependencies'])
    # The snapshot's case lookup must reproduce every frozen input byte; no task regeneration.
    module_spec = importlib.util.spec_from_file_location('frozen_oracle', oracle)
    module = importlib.util.module_from_spec(module_spec)
    sys.path.insert(0, str(snapshots))
    try:
        module_spec.loader.exec_module(module)
        for case in cases.values():
            cli.require(module.study.case(case['case_id']) == case, 'oracle/corpus task mismatch')
    finally:
        sys.path.pop(0)
    schedule = {'schema_version': 1, 'corpus_file': str(corpus_path),
                'corpus_sha256': plan['corpus_sha256'], 'subject': subject,
                'base_job_sha256': cli.digest(plan['base_job']), 'trials': [],
                'configuration_complete': False}
    bindings = plan.get('bindings', {})
    for pair in corpus['pairs']:
        case = cases[pair['case_id']]
        for arm in pair['arms']:
            identity = case['case_id'] + '/repetition-' + str(pair['repetition']) + '/' + arm
            trial = {'id': identity, 'case_id': case['case_id'], 'feature': case['feature'],
                     'control': case['control'], 'arm': arm, 'repetition': pair['repetition'],
                     'metric': 'coding_task_success', 'gap': None,
                     'treatment': 'single' if arm == 'on' else 'all_off',
                     'contrast_id': 'single_vs_all_off'}
            folder = root / identity.replace('/', '__')
            folder.mkdir()
            trial['receipt_file'] = str(folder / 'admission-receipt.json')
            trial['result_file'] = str(folder / 'result.json')
            if identity in bindings:
                binding = bindings[identity]
                cli.require(binding['adapter'] in ADAPTERS and cli.digest(binding['job_file']) == binding['job_sha256'],
                            'actual adapter binding mismatch')
                trial.update(binding)
                bound_job = cli.read_json(binding['job_file'])
                if binding['adapter'] == 'navigation':
                    bound_jobs = [cli.read_json(card['job_file']) for card in bound_job['cards']]
                else:
                    bound_jobs = [bound_job]
                cli.require(all(j.get('subject_binary') == subject['executable'] and
                                j.get('subject_sha256') == subject['sha256'] for j in bound_jobs),
                            'binding subject mismatch')
                if case['control'] == 'outage':
                    cli.require(binding.get('outage_verified') is True and binding.get('outage_evidence_sha256'),
                                'real outage configuration evidence required')
            elif case['feature'] not in {'tool_result_triage', 'progress_monitoring', 'recommendations'} or case['control'] == 'outage':
                trial['gap'] = 'exact_actual_adapter_or_verified_outage_binding_required'
            else:
                report_dir = folder / 'reports'
                state = folder / 'state'
                for path in (report_dir, state, state / 'sessions', folder / 'tmp'):
                    path.mkdir()
                starter = folder / 'starter.json'
                cli.write_json(starter, case['files'])
                template = copy.deepcopy(template_base)
                template['request'].update(feature=case['feature'], arm=arm, task_id=case['case_id'],
                    repetition=pair['repetition'], capture_id=identity, pairing_id=identity.rsplit('/', 1)[0],
                    isolated_feature=True, source='live_consumer')
                template['request']['subject'].update(executable=subject['executable'],
                    executable_sha256=subject['sha256'], artifact_id=subject['artifact_id'], configuration_complete=False)
                template['request']['subject'].pop('state_id', None)
                template['consumer']['request']['prompt'] = case['prompt']
                overlay = base['arm']['overlay_toml']
                import re
                for feature in normal.FEATURES:
                    value = 'auto' if arm == 'on' and feature == case['feature'] else 'off'
                    overlay, count = re.subn(r'(?m)^' + re.escape(feature) + r'[ \t]*=[ \t]*"(?:auto|off)"[ \t]*$',
                                             feature + ' = "' + value + '"', overlay)
                    cli.require(count == 1, 'explicit feature overlay replacement required')
                template_path = folder / 'capture-template.json'
                cli.write_json(template_path, template)
                job = copy.deepcopy(base)
                job.update(subject_binary=subject['executable'], subject_sha256=subject['sha256'],
                    artifact_id=subject['artifact_id'], run_directory=str(report_dir),
                    capture_template=str(template_path), starter_files=str(starter), scenario_id=case['case_id'],
                    artifact=case['artifact'], oracle_module=oracle, oracle_sha256=oracle_hash,
                    oracle_dependencies=dependencies, control=case['control'], resolve_state_identity=True,
                    treatment='single' if arm == 'on' else 'all_off')
                job.update(required_hooks={case['feature']: 'applied' if arm == 'on' and case['control'] == 'positive'
                                           else 'disabled' if arm == 'off' else 'baseline_retained'})
                job['arm'].update(arm=arm, state_root=str(state), overlay_toml=overlay)
                job_path = folder / 'job.json'
                cli.write_json(job_path, job)
                trial.update(adapter='normal', job_file=str(job_path), job_sha256=cli.digest(job_path),
                             report_file=str(report_dir / 'normal-pilot.json'))
            schedule['trials'].append(trial)
    for trial in plan.get('combined_trials', []):
        cli.require(trial['adapter'] in ADAPTERS and cli.digest(trial['job_file']) == trial['job_sha256'],
                    'combined actual binding required')
        cli.require(trial.get('contrast_id') and trial.get('treatment'), 'explicit combined contrast/treatment required')
        combined = dict(trial)
        folder = root / ('combined-' + str(len(schedule['trials'])))
        folder.mkdir()
        combined.update(receipt_file=str(folder / 'admission-receipt.json'), result_file=str(folder / 'result.json'))
        schedule['trials'].append(combined)
    context_receipts = {}
    for trial in schedule['trials']:
        if trial.get('adapter') == 'context' and not trial.get('gap'):
            pair_id = trial.get('admission_pair_id')
            cli.require(isinstance(pair_id, str) and pair_id, 'context pair identity required')
            trial['receipt_file'] = context_receipts.setdefault(pair_id, trial['receipt_file'])
    validate_context_pairs(schedule['trials'])
    schedule['driver_sha256'] = cli.digest(__file__)
    schedule['adapter_sha256'] = {name: cli.digest(Path(__file__).with_name('judgement-' + name + '-live.py'))
                                  for name in ('normal', 'context', 'navigation')}
    cli.write_json(root / 'schedule.json', schedule)
    return schedule


def oracle_artifact(binding):
    """Bounded explicit evidence locator; no inferred/private storage paths."""
    path = Path(binding['path'])
    cli.require(path.is_absolute() and not path.is_symlink(), 'explicit oracle artifact required')
    cli.external_path(str(path.parent))
    cli.require(cli.digest(path) == binding['sha256'] and path.stat().st_size <= 256 * 1024,
                'bounded oracle artifact identity mismatch')
    value = cli.read_json(path)
    cli.require(value.get('schema_version') == 1 and type(value.get('passed')) is bool,
                'typed oracle artifact required')
    return value


def context_arm_report(report, trial):
    cli.require(trial.get('admission_pair_id') and trial.get('report_arm') == trial['arm'],
                'explicit context pair/arm binding required')
    arms = report.get('arms')
    cli.require(isinstance(arms, list) and len(arms) == 2
                and {a.get('arm') for a in arms} == {'off', 'on'}, 'exact context pair report required')
    return next(a for a in arms if a['arm'] == trial['arm'])


def context_pair_terminal(report):
    arms = report.get('arms', [])
    return (len(arms) == 2 and {a.get('arm') for a in arms} == {'off', 'on'}
            and all(a.get('terminal_proof', {}).get('verified') is True
                    and (a.get('capture') or {}).get('outcome') in ('succeeded', 'failed', 'cancelled')
                    for a in arms))


def validate_context_pairs(trials):
    pairs = {}
    for trial in trials:
        if trial.get('adapter') == 'context' and not trial.get('gap'):
            cli.require(trial.get('admission_pair_id'), 'context requires pair-level admission')
            pairs.setdefault(trial['admission_pair_id'], []).append(trial)
    for pair in pairs.values():
        cli.require(len(pair) == 2 and {t['arm'] for t in pair} == {'off', 'on'}
                    and len({(t['job_file'], t['job_sha256'], t['report_file'], t['receipt_file']) for t in pair}) == 1
                    and len({(t['case_id'], t['repetition']) for t in pair}) == 1,
                    'context pair must share exact job/report/receipt')


def summarize(report, trial):
    if trial['adapter'] == 'context':
        report = context_arm_report(report, trial)
    metric = trial['metric']
    capture = report.get('capture', report if trial['adapter'] == 'eval' else {}) or {}
    oracle = report.get('oracle') or {}
    artifact_gap = None
    if trial['adapter'] == 'eval':
        if trial.get('oracle_artifact'):
            artifact = oracle_artifact(trial['oracle_artifact'])
            oracle = {'status': 'pass' if artifact['passed'] else 'fail'}
            if artifact.get('capture_complete') is not True:
                artifact_gap = 'independent_eval_capture_incomplete'
        else:
            artifact_gap = 'independent_eval_oracle_artifact_required'
    task = oracle.get('status') if metric == 'coding_task_success' else None
    # Label/navigation truth requires explicit native consumer facts supplied by its owning adapter.
    native = report.get('native_metric') or {}
    truth = task if metric == 'coding_task_success' else native.get('status')
    gap = report.get('gap') or artifact_gap
    if capture.get('status') in ('public_operation_failed', 'timed_out', 'consumer_unavailable'):
        gap = gap or 'actual_consumer_evidence_unavailable'
    if metric != 'coding_task_success' and native.get('scope') != 'product_native':
        truth = None
        gap = gap or 'product_native_metric_required'
    if truth not in ('pass', 'fail'):
        gap = gap or 'independent_metric_unavailable'
    telemetry = capture.get('telemetry') or {}
    hooks = report.get('hook_evidence') or {}
    return {'id': trial['id'], 'metric': metric, 'truth': truth, 'gap': gap,
            'hook_application_satisfied': hooks.get('all_required_satisfied'),
            'hook_evidence_sha256': hooks.get('observation_sha256'),
            'control': trial.get('control'),
            'task_truth': task, 'latency_ms': capture.get('trial_latency_ms'),
            'optional_complete': telemetry.get('optional_operations_complete') is True,
            'inference_calls': telemetry.get('inference_calls'),
            'capture_status': capture.get('status'), 'scope': capture.get('scope')}


def reconcile(schedule):
    for trial in schedule['trials']:
        if trial.get('gap'):
            continue
        report_path = Path(trial['report_file'])
        if report_path.is_file():
            result = summarize(cli.read_json(report_path), trial)
            result['report_sha256'] = cli.digest(report_path)
            atomic(Path(trial['result_file']), result)


def adapter_diagnostic(stream, returncode):
    """Retain bounded allowlisted stderr only; arbitrary provider/traceback text is withheld."""
    stream.seek(0)
    raw = stream.read(4097)
    allowed = (b'Pilot preflight missing source_order; consumer not started.\n',
               b'Pilot schema prerequisite missing; execution effects unknown.\n',
               b'Pilot precondition failed; no configuration/effectiveness attestation.\n')
    return {'returncode': returncode, 'stderr': raw.decode('ascii') if raw in allowed else
            'Untrusted adapter stderr withheld; execution effects unknown.',
            'stderr_truncated': len(raw) > 4096,
            'consumer_not_started': raw == allowed[0]}


def require_settled_admissions(schedule):
    """Fail closed before source rebinding or admitting more work, including filtered runs."""
    for trial in schedule['trials']:
        receipt_path = Path(trial['receipt_file'])
        if not receipt_path.exists():
            continue
        receipt = cli.read_json(receipt_path)
        cli.require(receipt.get('id') == trial.get('admission_pair_id', trial['id'])
                    and receipt.get('job_sha256') == trial.get('job_sha256')
                    and trial.get('job_file') and cli.digest(trial['job_file']) == trial['job_sha256'],
                    'receipt/job correlation mismatch')
        report_path = Path(trial['report_file'])
        cli.require(report_path.is_file(), 'unresolved prior admission; no replay or continuation')
        report = cli.read_json(report_path)
        if trial.get('adapter') == 'context':
            cli.require(context_pair_terminal(report),
                        'context pair terminal proof missing; no replay or continuation')
            context_arm_report(report, trial)
        elif trial.get('adapter') == 'normal':
            capture = report.get('capture') or {}
            cli.require(capture.get('outcome') in ('succeeded', 'failed', 'cancelled')
                        and capture.get('status') not in ('timed_out', 'public_operation_failed')
                        and report.get('turn_id')
                        and (report.get('history_after') or {}).get('complete') is True,
                        'bounded canonical terminal proof missing; no replay or continuation')
        else:
            # Eval/navigation reports do not yet expose correlated terminal execution proof.
            cli.require(False, 'adapter terminal correlation unsupported; no replay or continuation')


def rebind_resume(schedule, output):
    """Explicit source revision after bounded proof of every prior admission's terminal capture."""
    cli.require(not output.exists(), 'fresh rebound schedule required')
    cli.external_path(str(output.parent))
    cli.require(cli.digest(schedule['corpus_file']) == schedule['corpus_sha256'], 'corpus changed')
    validate_context_pairs(schedule['trials'])
    require_settled_admissions(schedule)
    reconcile(schedule)
    rebound = copy.deepcopy(schedule)
    rebound['previous_driver_sha256'] = schedule.get('driver_sha256')
    rebound['driver_sha256'] = cli.digest(__file__)
    rebound['adapter_sha256'] = {name: cli.digest(Path(__file__).with_name('judgement-' + name + '-live.py'))
                                for name in ('normal', 'context', 'navigation')}
    rebound['resume_reconciliation'] = 'retained_terminal_captures_no_replay'
    cli.write_json(output, rebound)
    return rebound


def run(schedule, limit, features=None, controls=None, repetitions=None):
    cli.require(type(limit) is int and 0 < limit <= len(schedule['trials']), 'finite admission limit required')
    cli.require(cli.digest(schedule['corpus_file']) == schedule['corpus_sha256'], 'corpus changed')
    validate_context_pairs(schedule['trials'])
    require_settled_admissions(schedule)
    if schedule.get('driver_sha256'):
        cli.require(cli.digest(__file__) == schedule['driver_sha256'], 'driver changed after freeze')
        cli.require(all(cli.digest(Path(__file__).with_name('judgement-' + name + '-live.py')) == digest
                        for name, digest in schedule['adapter_sha256'].items()), 'adapter changed after freeze')
    reconcile(schedule)
    admitted = 0
    for trial in schedule['trials']:
        if admitted >= limit:
            break
        if features and trial['feature'] not in features:
            continue
        if controls and trial.get('control') not in controls:
            continue
        if repetitions is not None and trial['repetition'] not in repetitions:
            continue
        if trial.get('gap') or Path(trial['result_file']).exists():
            continue
        require_settled_admissions(schedule)
        receipt = Path(trial['receipt_file'])
        cli.require(not receipt.exists(), 'unreconciled admission; never blindly repeat')
        cli.require(cli.digest(trial['job_file']) == trial['job_sha256'], 'job changed before admission')
        cli.require(cli.digest(schedule['subject']['executable']) == schedule['subject']['sha256'], 'subject changed')
        try:
            cli.write_json(receipt, {'id': trial.get('admission_pair_id', trial['id']), 'job_sha256': trial['job_sha256'],
                                     'phase': 'admission_may_have_effects', 'automatic_retry': False})
        except FileExistsError as error:
            raise cli.PilotError('admission conflict; receipt already claimed; no dispatch') from error
        admitted += 1
        job = cli.read_json(trial['job_file'])
        if trial['adapter'] == 'eval':
            # Exact subject CLI and environment are provided by the coordinator's pinned eval binding.
            raw = cli.command(job, job['arm'], ['judgement', 'benchmark-capture', '--file', job['capture_template']],
                              job['run_directory'], timeout=330)
            cli.write_json(Path(trial['report_file']), json.loads(raw))
            reconcile(schedule)
            continue
        script = Path(__file__).with_name('judgement-' + trial['adapter'] + '-live.py')
        with tempfile.TemporaryFile() as stderr:
            try:
                completed = subprocess.run([sys.executable, '-B', str(script), '--file', trial['job_file']],
                                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                           stderr=stderr, timeout=600, check=False)
            except subprocess.TimeoutExpired:
                atomic(receipt.with_suffix('.diagnostic.json'), adapter_diagnostic(stderr, None))
                raise
            if completed.returncode != 0:
                atomic(receipt.with_suffix('.diagnostic.json'), adapter_diagnostic(stderr, completed.returncode))
        reconcile(schedule)
        if completed.returncode != 0:
            break  # already-retained failure is a result; unretained effects require reconciliation
    return reduce(schedule)


def reduce(schedule):
    groups, pairs, trials_report = {}, {}, []
    for trial in schedule['trials']:
        group = groups.setdefault(trial['feature'], {'observed': 0, 'gaps': 0, 'pass': 0, 'fail': 0,
            'assigned': 0, 'intent_to_treat_pass': 0, 'intent_to_treat_fail': 0, 'intent_to_treat_unknown': 0,
            'paired_wins': 0, 'paired_losses': 0, 'paired_ties': 0,
            'enhancement_qualified_pairs': 0, 'enhancement_unqualified_pairs': 0,
            'qualified_wins': 0, 'qualified_losses': 0, 'qualified_ties': 0, 'latency_ratios': []})
        group['assigned'] += 1
        path = Path(trial['result_file'])
        result = cli.read_json(path) if path.exists() else {
            'gap': 'admitted_without_report' if trial.get('receipt_file') and Path(trial['receipt_file']).exists()
                   else trial.get('gap') or 'not_admitted'}
        truth = result.get('task_truth') or result.get('truth')
        group['intent_to_treat_' + (truth if truth in ('pass', 'fail') else 'unknown')] += 1
        trials_report.append({'id': trial.get('id'), 'treatment': trial.get('treatment'),
                              'contrast_id': trial.get('contrast_id', 'single_vs_all_off'), **result})
        if result.get('gap') or result.get('truth') not in ('pass', 'fail') or not result.get('optional_complete'):
            group['gaps'] += 1
            continue
        group['observed'] += 1
        group[result['truth']] += 1
        key = (trial['feature'], trial['case_id'], trial['repetition'],
               trial.get('metric', 'coding_task_success'), trial.get('contrast_id', 'single_vs_all_off'))
        pair = pairs.setdefault(key, {})
        cli.require(trial['arm'] not in pair, 'duplicate comparison arm; explicit contrast identity required')
        pair[trial['arm']] = (trial, result)
    for pair in pairs.values():
        if set(pair) != {'on', 'off'}:
            continue
        on_trial, on = pair['on']
        _, off = pair['off']
        group = groups[on_trial['feature']]
        outcome = 'ties' if on['truth'] == off['truth'] else 'wins' if on['truth'] == 'pass' else 'losses'
        group['paired_' + outcome] += 1
        qualified = on.get('hook_application_satisfied') is True and off.get('hook_application_satisfied') is True
        group['enhancement_qualified_pairs' if qualified else 'enhancement_unqualified_pairs'] += 1
        if qualified:
            group['qualified_' + outcome] += 1
        if isinstance(off.get('latency_ms'), (int, float)) and off['latency_ms'] > 0 and isinstance(on.get('latency_ms'), (int, float)):
            group['latency_ratios'].append(on['latency_ms'] / off['latency_ms'])
    return {'schema_version': 2, 'groups': groups, 'trials': trials_report, 'all_trials': len(schedule['trials']),
            'acceptance_claimed': False, 'corpus_sha256': schedule['corpus_sha256']}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('action', choices=('materialize', 'run', 'reconcile', 'reduce', 'rebind-resume'))
    parser.add_argument('--file', required=True, type=Path)
    parser.add_argument('--limit', type=int)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--feature', action='append', choices=sorted(normal.FEATURES))
    parser.add_argument('--control', action='append', choices=('positive', 'negative', 'outage'))
    parser.add_argument('--repetition', action='append', type=int)
    args = parser.parse_args()
    value = cli.read_json(args.file)
    try:
        if args.action == 'materialize':
            result = materialize(value)
            print(json.dumps({'trials': len(result['trials']), 'supported': sum(not t.get('gap') for t in result['trials'])}))
        elif args.action == 'rebind-resume':
            cli.require(args.output is not None, 'explicit rebound schedule output required')
            rebind_resume(value, args.output)
            print(json.dumps({'rebound_schedule': str(args.output), 'replayed': False}))
        elif args.action == 'run':
            print(json.dumps(run(value, args.limit, args.feature, args.control, args.repetition), sort_keys=True))
        elif args.action == 'reconcile':
            reconcile(value)
            print(json.dumps(reduce(value), sort_keys=True))
        else:
            print(json.dumps(reduce(value), sort_keys=True))
    except cli.PilotError as error:
        parser.exit(1, str(error) + '\n')
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired):
        parser.exit(1, 'Study operation incomplete; retain receipts and reconcile, never blind replay.\n')


if __name__ == '__main__':
    raise SystemExit(main())
