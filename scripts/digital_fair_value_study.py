#!/usr/bin/env python3
"""Offline, frozen-boundary probability study. Python standard library only."""
import argparse
import hashlib
import json
import math
from pathlib import Path
from collections import defaultdict
from datetime import datetime, timezone


def timestamp(value):
    dt = datetime.fromisoformat(value.replace('Z', '+00:00'))
    if dt.tzinfo is None:
        raise ValueError('timestamps must include timezone')
    return int(dt.timestamp())


def partition(t, config):
    if config.get('frozen_test'):
        start, end = timestamp(config['start']), timestamp(config['end'])
        if start >= end:
            raise ValueError('frozen-test boundaries must be strictly increasing')
        return 'frozen_test' if start <= t < end else None
    bounds = [timestamp(config[k]) for k in ('start', 'train_end', 'validation_end', 'end')]
    if bounds != sorted(set(bounds)):
        raise ValueError('split boundaries must be strictly increasing')
    for i, name in enumerate(('train', 'validation', 'historical_review')):
        if bounds[i] <= t < bounds[i + 1]:
            return name
    return None


def finite(x):
    try:
        x = float(x)
        return x if math.isfinite(x) else None
    except (ValueError, TypeError):
        return None


def score(pairs):
    if not pairs:
        return {'samples': 0, 'brier': None, 'log_loss': None}
    n = len(pairs)
    return {'samples': n, 'wins': sum(y for p, y in pairs),
            'mean_probability': sum(p for p, y in pairs) / n,
            'win_rate': sum(y for p, y in pairs) / n,
            'brier': sum((p-y)**2 for p, y in pairs) / n,
            'log_loss': -sum(math.log(max(1e-6, min(1-1e-6, p if y else 1-p))) for p, y in pairs) / n}


def predict(row, model):
    kind = model['kind']
    if kind == 'current':
        return row['p']
    if kind == 'without_floor':
        return row['unfloored']
    if kind == 'floor':
        sigma = max(row['observed_vol'], row['floor'] * model['scale']) * math.sqrt(row['remaining'])
        return min(1-1e-6, max(1e-6, .5 * (1 + math.erf(row['margin'] / max(sigma, 1e-6) / math.sqrt(2)))))
    # A training-only empirical distribution of final/current log returns.
    # Add-one smoothing bounds confidence even beyond observed tails.
    threshold = math.log(row['opening'] / row['current']) * 10000
    residuals = model['residuals_bps']
    return (1 + sum(r >= threshold for r in residuals)) / (len(residuals) + 2)


def load(config, root):
    rows, provenance, seen, labels_seen = [], [], {}, {}
    excluded = defaultdict(int)
    for source in config['inputs']:
        path = (root / source).resolve()
        cache_path = path.parent / 'reference-audit.json'
        raw_cache = cache_path.read_bytes()
        cache = json.loads(raw_cache)
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for line_no, line in enumerate(stream, 1):
                digest.update(line)
                if not line.strip():
                    continue
                try:
                    e = json.loads(line)
                except ValueError as error:
                    raise ValueError(f'{path}:{line_no}: {error}') from error
                if e.get('event') != 'model_observation':
                    continue
                slug, h = e['slug'], int(e['checkpoint_secs'])
                t = int(slug.rsplit('-', 1)[1])
                split = partition(t, config)
                if split is None or h not in config['checkpoints']:
                    excluded['outside_scope'] += 1
                    continue
                label = cache.get(slug, {})
                if label.get('confirmation') != 'closed_terminal_prices' or not isinstance(label.get('winner_up'), bool):
                    excluded['missing_verified_label'] += 1
                    continue
                if slug in labels_seen and labels_seen[slug] != label['winner_up']:
                    raise ValueError(f'conflicting labels: {slug}')
                labels_seen[slug] = label['winner_up']
                audit = e.get('model_audit', {})
                fields = dict(p=e.get('probability_up'), unfloored=audit.get('probability_without_floor'),
                              observed_vol=audit.get('observed_volatility_bps'), floor=audit.get('minimum_volatility_bps'),
                              margin=audit.get('directional_margin_bps'), remaining=e.get('seconds_remaining'),
                              opening=e.get('opening_twap'), current=e.get('current_twap'), final=label.get('final_price'))
                fields = {k: finite(v) for k, v in fields.items()}
                if any(v is None for v in fields.values()) or any(fields[k] <= 0 for k in ('opening','current','final','remaining','floor')) or fields['observed_vol'] < 0 or not all(0 <= fields[k] <= 1 for k in ('p','unfloored')):
                    excluded['invalid_or_missing_features'] += 1
                    continue
                if e.get('reference_topic') != 'crypto_prices_twap_sixty':
                    excluded['incompatible_reference_topic'] += 1
                    continue
                row = dict(fields, slug=slug, checkpoint=h, asset=e['asset'], time=t, split=split,
                           y=int(label['winner_up']), market=finite(e.get('market_probability_up')),
                           age=finite(audit.get('feed_age_ms')),
                           observed_at=timestamp(e['recorded_at']), source=source)
                key = (slug, h)
                if key in seen:
                    excluded['duplicate_observations'] += 1
                    if row['observed_at'] >= seen[key]['observed_at']:
                        continue
                seen[key] = row
        provenance.append({'journal': str(path), 'sha256': digest.hexdigest(),
                           'labels_sha256': hashlib.sha256(raw_cache).hexdigest()})
    rows = sorted(seen.values(), key=lambda r: (r['time'], r['asset'], r['checkpoint']))
    return rows, provenance, dict(excluded)


def fit(rows):
    groups = defaultdict(list)
    for row in rows:
        groups[f"{row['asset']}/{row['checkpoint']}s"].append(row)
    models = {}
    for group, values in groups.items():
        train = [r for r in values if r['split'] == 'train']
        val = [r for r in values if r['split'] == 'validation']
        if len(train) < 50 or len(val) < 20:
            models[group] = {'status': 'insufficient_samples', 'train_n': len(train), 'validation_n': len(val)}
            continue
        # Fixed, small family; select floor scale on training only.
        scales = [.25, .5, 1.0]
        scale = min(scales, key=lambda s: score([(predict(r, {'kind':'floor','scale':s}), r['y']) for r in train])['log_loss'])
        candidates = {'current': {'kind':'current'}, 'floor': {'kind':'floor','scale':scale},
                      'empirical': {'kind':'empirical', 'residuals_bps': sorted(math.log(r['final']/r['current'])*10000 for r in train)}}
        validation_scores = {k: score([(predict(r, m), r['y']) for r in val]) for k, m in candidates.items()}
        baseline = validation_scores['current']
        acceptable = [k for k,v in validation_scores.items() if v['brier'] <= baseline['brier'] and v['log_loss'] <= baseline['log_loss']]
        selected = min(acceptable, key=lambda k: validation_scores[k]['log_loss'])
        models[group] = {'status': 'fitted', 'train_n':len(train), 'validation_n':len(val),
                         'candidates':candidates, 'selected':selected, 'validation_scores':validation_scores}
    return models


def evaluate(rows, models):
    buckets = defaultdict(list)
    daily_deltas = defaultdict(lambda: defaultdict(list))
    for r in rows:
        group = f"{r['asset']}/{r['checkpoint']}s"
        model = models.get(group, {})
        candidates = dict(model.get('candidates', {'current':{'kind':'current'}}), without_floor={'kind':'without_floor'})
        if model.get('status') == 'fitted':
            candidates['selected'] = model['candidates'][model['selected']]
        for name, candidate in candidates.items():
            p = predict(r, candidate)
            base = f"{r['split']}/{group}/{name}"
            buckets[base+'/all'].append((p,r['y']))
            age = r['age']
            age_group = 'unknown' if age is None or age < 0 else '0_1s' if age<=1000 else '1_2s' if age<=2000 else '2_3s' if age<=3000 else '3_5s' if age<=5000 else 'gt_5s'
            buckets[base+'/age/'+age_group].append((p,r['y']))
            for side, q, y in [('up', p, r['y']), ('down',1-p,1-r['y'])]:
                buckets[base+f'/calibration/{side}/{min(9,int(q*10))}'].append((q,y))
            m = r['market']
            if m is not None and 0 <= m <= 1:
                buckets[base+'/paired_model'].append((p,r['y']))
                buckets[base+'/paired_market'].append((m,r['y']))
                # Direction chosen from model-vs-market difference at observation
                # time; this is a forecast diagnostic, never an execution PnL.
                up = p >= m
                q, market_q, y = (p,m,r['y']) if up else (1-p,1-m,1-r['y'])
                edge_bin = 'ge_0_10' if q-market_q >= .1 else '0_03_0_10' if q-market_q >= .03 else 'below_0_03'
                buckets[base+'/edge/'+edge_bin+'/model'].append((q,y))
                buckets[base+'/edge/'+edge_bin+'/market'].append((market_q,y))
                day = datetime.fromtimestamp(r['time'],timezone.utc).date().isoformat()
                daily_deltas[base][day].append((p-r['y'])**2-(m-r['y'])**2)
    return {'scores':{k:score(v) for k,v in sorted(buckets.items())},
            'paired_brier_delta_by_day':{k:{day:{'samples':len(v),'mean_delta':sum(v)/len(v)} for day,v in days.items()} for k,days in daily_deltas.items()},
            'uncertainty':'Days are correlated blocks; fewer than several independent days cannot establish a robust advantage.',
            'execution_gate':{'status':'NOT_EVALUABLE', 'reason':'Checkpoint midpoints do not provide complete delayed executable books for alternative signals. No counterfactual PnL fabricated.'},
            'promotion':'BLOCKED_PENDING_FRESH_DATA_AND_EXECUTION_VALIDATION'}


def frozen_probability_gate(result, models, policy):
    checks, incomplete, failed = {}, False, False
    split = 'frozen_test'
    for group, model in models.items():
        if not group.endswith('/30s') or model.get('status') != 'fitted':
            continue
        prefix = f'{split}/{group}/'
        candidate = result['scores'].get(prefix+'selected/paired_model', score([]))
        market = result['scores'].get(prefix+'selected/paired_market', score([]))
        current = result['scores'].get(prefix+'current/paired_model', score([]))
        days = len(result['paired_brier_delta_by_day'].get(prefix+'selected', {}))
        enough = candidate['samples'] >= policy['min_paired_samples_per_asset'] and days >= policy['min_utc_days']
        advantage = (enough and candidate['brier'] < market['brier']
                     and candidate['log_loss'] < market['log_loss']
                     and candidate['brier'] < current['brier']
                     and candidate['log_loss'] < current['log_loss'])
        checks[group] = {'paired_samples':candidate['samples'], 'utc_days':days,
                         'candidate':candidate, 'market':market, 'current':current,
                         'enough_data':enough, 'passes_probability_advantage':advantage}
        incomplete |= not enough
        failed |= enough and not advantage
    status = 'INCOMPLETE' if incomplete else 'FAILED' if failed else 'PASSED'
    if not checks:
        status = 'INCOMPLETE'
    return {'status':status, 'policy':policy, 'checks':checks,
            'scope':'Probability quality only; execution profitability remains a separate blocked gate.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='new output directory; existing files are never overwritten')
    parser.add_argument('--model', type=Path, help='evaluate a previously frozen model without fitting')
    args = parser.parse_args()
    config = json.loads(args.manifest.read_text())
    root = (args.manifest.parent / config.get('root', '..')).resolve()
    rows, provenance, exclusions = load(config, root)
    if not rows:
        raise ValueError('no usable observations')
    if args.model:
        artifact = json.loads(args.model.read_text())
        if not config.get('frozen_test'):
            raise ValueError('a frozen model requires a manifest with frozen_test=true')
        if timestamp(config['start']) < timestamp(artifact['manifest']['end']):
            raise ValueError('frozen evaluation must start after the complete development/review period')
        models = artifact['models']
    else:
        models = fit(rows)
    result = evaluate(rows, models)
    if args.model:
        result['probability_gate'] = frozen_probability_gate(
            result, models, config['probability_gate'])
    args.output.mkdir(parents=True, exist_ok=False)
    payload = {'schema_version':1, 'manifest':config, 'provenance':provenance, 'exclusions':exclusions,
               'models':models, 'observations':len(rows), 'result':result,
               'mode':'frozen_evaluation' if args.model else 'historical_development',
               'parent_model_sha256':hashlib.sha256(args.model.read_bytes()).hexdigest() if args.model else None}
    (args.output/'study.json').write_text(json.dumps(payload, indent=2, allow_nan=False)+'\n')
    if not args.model:
        (args.output/'model.json').write_text(json.dumps({'schema_version':1,'manifest':config,'models':models,'provenance':provenance},indent=2)+'\n')
    lines = ['# Probability calibration study', '', f'Observations: {len(rows)}; mode: {payload["mode"]}.', '',
             '| Group | Selected | Training | Validation |','|---|---|---:|---:|']
    for group, model in models.items():
        lines.append(f'| {group} | {model.get("selected",model["status"])} | {model["train_n"]} | {model["validation_n"]} |')
    review_split = 'frozen_test' if args.model else 'historical_review'
    lines.extend(['', '| Evaluation group | Paired N | Current Brier | Selected Brier | Market Brier |', '|---|---:|---:|---:|---:|'])
    for group in models:
        prefix = review_split+'/'+group+'/'
        a = result['scores'].get(prefix+'current/paired_model')
        b = result['scores'].get(prefix+'selected/paired_model')
        c = result['scores'].get(prefix+'current/paired_market')
        if a and b and c:
            lines.append(f'| {group} | {a["samples"]} | {a["brier"]:.5f} | {b["brier"]:.5f} | {c["brier"]:.5f} |')
    if args.model:
        lines.extend(['', 'Probability gate: '+result['probability_gate']['status']+'.'])
    else:
        lines.extend(['', 'Historical review is already inspected data, not independent out-of-sample proof.'])
    lines.extend(['Execution gate: NOT_EVALUABLE. Promotion: BLOCKED.', '', 'Detailed paired, age and both-side calibration scores: study.json.'])
    (args.output/'report.md').write_text('\n'.join(lines)+'\n')
    print('\n'.join(lines))
    print(f'output={args.output}')


if __name__ == '__main__':
    main()
