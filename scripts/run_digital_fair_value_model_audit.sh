#!/usr/bin/env bash
set -euo pipefail

# Keep the live Gaussian decision unchanged while collecting frozen empirical,
# market-anchored and strict-subset counterfactual evidence. Research only.
project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

action=${1:-dry-run}
batch=${2:-research-model-audit-005}
case "$action" in dry-run|report|calibrate|validate) ;; *) echo 'usage: run_digital_fair_value_model_audit.sh [dry-run|report|calibrate|validate] [batch]' >&2; exit 2;; esac
if [[ "$action" == dry-run && "$batch" =~ ^research-model-audit-00[1-4]$ ]]; then
    echo 'research-model-audit-001..004 use older evidence schemas; run a new batch (default: research-model-audit-005)' >&2
    exit 2
fi
model=results/short-duration-digital-fair-value/study-006-007-v2/model.json
expected_sha=d0de136f4eb66f302e8f2d3d375a88aafc560a16f0fd7d0bd6812fc643d068f1
if [[ "$action" == dry-run ]]; then
    [[ -f "$model" ]] || { echo "missing research model: $model" >&2; exit 2; }
    actual_sha=$(sha256sum "$model" | awk '{print $1}')
    [[ "$actual_sha" == "$expected_sha" ]] || { echo "research model hash mismatch: $actual_sha" >&2; exit 2; }
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_FILE="$model"
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_SHA256="$expected_sha"
    export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS=${SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS:-2000}
fi
exec bash scripts/run_digital_fair_value_research.sh 30 "$batch" "$action"
