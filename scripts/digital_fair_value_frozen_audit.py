#!/usr/bin/env python3
"""Read-only coverage and journal-integrity audit for a frozen SDDFV run."""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
import hashlib
import json
from pathlib import Path


def utc_seconds(value):
    result = datetime.fromisoformat(value.replace('Z', '+00:00'))
    if result.tzinfo is None:
        raise ValueError('UTC boundary is missing a timezone')
    return result.timestamp()


def audit(manifest_path, model_path):
    manifest = json.loads(manifest_path.read_text())
    model = json.loads(model_path.read_text())
    root = (manifest_path.parent / manifest.get('root', '..')).resolve()
    start, end = utc_seconds(manifest['start']), utc_seconds(manifest['end'])
    checkpoints = set(manifest['checkpoints'])
    required = {
        group.split('/')[0]
        for group, spec in model['models'].items()
        if spec.get('status') == 'fitted'
        and int(group.rsplit('/', 1)[1].removesuffix('s')) in checkpoints
    }
    observations = {}
    counts, duplicates = Counter(), 0
    run_ids = set()
    sources = []
    for source in manifest['inputs']:
        path = (root / source).resolve()
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for line_no, line in enumerate(stream, 1):
                digest.update(line)
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except ValueError as error:
                    raise ValueError(f'{path}:{line_no}: {error}') from error
                counts[event.get('event', 'unknown')] += 1
                if event.get('run_id'):
                    run_ids.add(event['run_id'])
                if event.get('event') != 'model_observation':
                    continue
                slug = event['slug']
                checkpoint = int(event['checkpoint_secs'])
                market_time = int(slug.rsplit('-', 1)[1])
                if not start <= market_time < end or checkpoint not in checkpoints:
                    continue
                key = (slug, checkpoint)
                if key in observations:
                    duplicates += 1
                    continue  # same first-observation rule as the study
                observations[key] = event
        sources.append({'path': str(path), 'sha256': digest.hexdigest()})
    by_asset = defaultdict(lambda: {'observations': 0, 'paired': 0,
                                    'online_model_forecasts': 0, 'utc_days': set()})
    for (slug, _), event in observations.items():
        item = by_asset[event['asset']]
        item['observations'] += 1
        probability = event.get('market_probability_up')
        paired = isinstance(probability, (int, float)) and 0 <= probability <= 1
        if paired:
            item['paired'] += 1
            market_time = int(slug.rsplit('-', 1)[1])
            item['utc_days'].add(datetime.fromtimestamp(market_time, timezone.utc).date().isoformat())
        if isinstance(event.get('research_model'), dict):
            item['online_model_forecasts'] += 1
    policy = manifest['probability_gate']
    elapsed_end = min(end, datetime.now(timezone.utc).timestamp())
    elapsed_days = []
    day = datetime.fromtimestamp(start, timezone.utc).date()
    last_elapsed_day = datetime.fromtimestamp(max(start, elapsed_end - 1), timezone.utc).date()
    while day <= last_elapsed_day:
        elapsed_days.append(day.isoformat())
        day += timedelta(days=1)
    coverage = {}
    for asset in sorted(required):
        item = by_asset[asset]
        days = sorted(item['utc_days'])
        coverage[asset] = {'observations': item['observations'], 'raw_paired': item['paired'],
                           'online_model_forecasts': item['online_model_forecasts'],
                           'utc_days': days,
                           'uncovered_elapsed_utc_days': sorted(set(elapsed_days) - set(days)),
                           'enough_raw_coverage': item['paired'] >= policy['min_paired_samples_per_asset']
                           and len(days) >= policy['min_utc_days']}
    return {'status': 'RAW_COVERAGE_READY' if coverage and all(v['enough_raw_coverage'] for v in coverage.values())
            else 'INCOMPLETE', 'coverage': coverage, 'event_counts': dict(counts),
            'duplicate_observations': duplicates, 'run_ids': sorted(run_ids),
            'sources': sources,
            'note': 'Raw paired coverage is not a verified-label or profitability gate; study.py performs those checks.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--model', type=Path, required=True)
    parser.add_argument('--require-raw-coverage', action='store_true')
    args = parser.parse_args()
    result = audit(args.manifest, args.model)
    print(json.dumps(result, indent=2))
    if args.require_raw_coverage and result['status'] != 'RAW_COVERAGE_READY':
        raise SystemExit(2)


if __name__ == '__main__':
    main()
