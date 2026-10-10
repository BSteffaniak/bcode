#!/usr/bin/env python3
"""Coordinator-only live navigation study: --file MANIFEST.json.

Manifest v1: cards [{id, job_file, job_sha256}] (three in chronological display
order), query_labels {task, domain}, relevant_card_ids (independent original-
request hand labels), output_file. Card jobs use normal session_categorization
capture and independent artifact oracles. Ranking never sees relevance labels.
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


def rank_cards(cards, query):
    """Stable label-only adaptation; unavailable labels preserve chronology."""
    return sorted(cards, key=lambda card: -sum(
        card.get('labels', {}).get(key) == value for key, value in query.items()))


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
            update_owned_report(output, report)
        ranked = [card['id'] for card in rank_cards(report['cards'], query)]
        report.update(ranked_order=ranked, baseline_top1_relevant=ids[0] in relevant,
                      adapted_top1_relevant=ranked[0] in relevant,
                      benefit=ids[0] not in relevant and ranked[0] in relevant)
    except (ValueError, KeyError, TypeError, OSError, normal.subprocess.TimeoutExpired) as error:
        report['gap'] = str(error) if isinstance(error, cli.PilotError) else 'navigation evidence unavailable'
    update_owned_report(output, report)
    return 1 if report['gap'] else 0


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
