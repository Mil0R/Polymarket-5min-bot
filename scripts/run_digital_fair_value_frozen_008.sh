#!/usr/bin/env bash
set -euo pipefail

action=${1:-status}
project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

lock_file=scripts/digital_fair_value_frozen_008.lock.json
manifest=scripts/digital_fair_value_frozen_008.json
model=results/short-duration-digital-fair-value/study-006-007-v2/model.json
batch=server-low-latency-008
events=results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson
expected_model_sha=d0de136f4eb66f302e8f2d3d375a88aafc560a16f0fd7d0bd6812fc643d068f1
expected_manifest_sha=dac18267162603fea6247670ccbdf484c9ebc8a1e9f65389f3ad5fe563d228a5
binary=${SDDFV_BINARY:-./short_duration_digital_fair_value}

verify() {
    [[ -f "$lock_file" ]] || { echo "missing freeze lock: $lock_file" >&2; exit 2; }
    [[ -f "$manifest" ]] || { echo "missing frozen manifest: $manifest" >&2; exit 2; }
    [[ -f "$model" ]] || { echo "missing frozen model: $model" >&2; exit 2; }
    actual_model_sha=$(sha256sum "$model" | awk '{print $1}')
    [[ "$actual_model_sha" == "$expected_model_sha" ]] || {
        echo "frozen model hash mismatch: expected=$expected_model_sha actual=$actual_model_sha" >&2
        exit 2
    }
    actual_manifest_sha=$(sha256sum "$manifest" | awk '{print $1}')
    [[ "$actual_manifest_sha" == "$expected_manifest_sha" ]] || {
        echo "frozen manifest hash mismatch: expected=$expected_manifest_sha actual=$actual_manifest_sha" >&2
        exit 2
    }
    python3 -m json.tool "$lock_file" >/dev/null
    python3 -m json.tool "$manifest" >/dev/null
    echo "freeze verified model_sha256=$actual_model_sha manifest_sha256=$actual_manifest_sha checkpoint=30s batch=$batch"
}

within_period() {
    python3 - <<'PY'
import json
from datetime import datetime, timezone
c=json.load(open('scripts/digital_fair_value_frozen_008.json'))
now=datetime.now(timezone.utc)
start=datetime.fromisoformat(c['start'].replace('Z','+00:00'))
end=datetime.fromisoformat(c['end'].replace('Z','+00:00'))
if not start <= now < end:
    raise SystemExit(f'current UTC time {now.isoformat()} is outside frozen period [{start.isoformat()}, {end.isoformat()})')
PY
}

status() {
    verify
    if [[ ! -f "$events" ]]; then
        echo "status=NOT_STARTED events=$events"
        return
    fi
    EVENTS_FILE="$events" python3 - <<'PY'
import json, os
from collections import Counter
p=os.environ['EVENTS_FILE']; counts=Counter(); first=last=None; paired=0
for line in open(p):
    if not line.strip(): continue
    e=json.loads(line); counts[e.get('event','unknown')]+=1
    if e.get('event')=='model_observation':
        first=first or e.get('recorded_at'); last=e.get('recorded_at')
        paired += e.get('market_probability_up') is not None
print(f"status=COLLECTING observations={counts['model_observation']} paired={paired} fills={counts['shadow_fill']} resolutions={counts['resolution']} first={first} last={last}")
PY
}

case "$action" in
    verify)
        verify
        ;;
    status)
        status
        ;;
    start)
        verify
        within_period
        echo "starting frozen probability observation: batch=$batch checkpoint=30s"
        SDDFV_BINARY="$binary" exec bash scripts/run_digital_fair_value_research.sh 30 "$batch" dry-run
        ;;
    finalize)
        verify
        [[ -s "$events" ]] || { echo "no frozen events: $events" >&2; exit 2; }
        python3 - <<'PY'
import json
from datetime import datetime, timezone
c=json.load(open('scripts/digital_fair_value_frozen_008.json'))
end=datetime.fromisoformat(c['end'].replace('Z','+00:00'))
if datetime.now(timezone.utc) < end:
    raise SystemExit(f'frozen period has not ended; finalize after {end.isoformat()}')
PY
        SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" report
        SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" calibrate
        python3 scripts/digital_fair_value_study.py \
            --manifest "$manifest" \
            --model "$model" \
            --output results/short-duration-digital-fair-value/frozen-eval-008-final
        ;;
    *)
        echo "usage: $0 [verify|status|start|finalize]" >&2
        exit 2
        ;;
esac
