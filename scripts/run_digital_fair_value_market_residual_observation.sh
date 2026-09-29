#!/usr/bin/env bash
set -euo pipefail

# Run 007 captures a signal plus linked >=1s delayed REST depth per market.
# No order is simulated online. Offline study fits market probability residuals.
project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

action=${1:-dry-run}
batch=${2:-market-residual-execution-007}
case "$action" in
    dry-run|report|calibrate|validate|study) ;;
    *) echo 'usage: run_digital_fair_value_market_residual_observation.sh [dry-run|report|calibrate|validate|study] [batch]' >&2; exit 2 ;;
esac
[[ "$batch" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid batch' >&2; exit 2; }

if [[ "$action" == study ]]; then
    args=(--input "results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson"
          --output "${SDDFV_RESIDUAL_STUDY_OUTPUT:-results/short-duration-digital-fair-value/${batch}-residual-study}")
    # Providing a frozen model evaluates NEW later data only, without refitting.
    if [[ -n "${SDDFV_RESIDUAL_FROZEN_MODEL:-}" ]]; then
        args+=(--model "$SDDFV_RESIDUAL_FROZEN_MODEL")
    fi
    exec python3 scripts/digital_fair_value_residual_study.py "${args[@]}"
fi

model=results/short-duration-digital-fair-value/study-006-007-v2/model.json
expected_sha=d0de136f4eb66f302e8f2d3d375a88aafc560a16f0fd7d0bd6812fc643d068f1
if [[ "$action" == dry-run ]]; then
    # Optional historical empirical comparator, not a decision dependency.
    if [[ -f "$model" ]]; then
        actual_sha=$(sha256sum "$model" | awk '{print $1}')
        [[ "$actual_sha" == "$expected_sha" ]] || { echo "research model hash mismatch: $actual_sha" >&2; exit 2; }
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_FILE="$model"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_SHA256="$expected_sha"
    else
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_FILE=''
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_SHA256=''
        echo 'historical empirical model absent; residual signal/depth capture does not require it'
    fi
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_OBSERVATION_ONLY=true
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_EXECUTION_PROBE=true
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_EXECUTION_DELAY_MS=1000
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_CAPTURE_RAW_REFERENCE=false
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS=0
fi

exec bash scripts/run_digital_fair_value_research.sh 30 "$batch" "$action"
