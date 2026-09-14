#!/usr/bin/env bash
set -euo pipefail
# Run from the directory containing .env. One process per checkpoint.
checkpoint=${1:-10}
batch=${2:-server-low-latency-007}
command=${3:-dry-run}
case "$checkpoint" in 10|30|60) ;; *) echo 'checkpoint must be 10, 30 or 60' >&2; exit 2;; esac
case "$command" in dry-run|report|calibrate|validate) ;; *) echo 'command must be dry-run, report, calibrate or validate' >&2; exit 2;; esac
if [[ "$checkpoint" == 60 && "$command" == dry-run && "${SDDFV_ALLOW_60S_RESEARCH:-0}" != 1 ]]; then
    echo '60s order simulation is not recommended before model calibration; use 10/30s. Explicit research override: SDDFV_ALLOW_60S_RESEARCH=1' >&2
    exit 2
fi
[[ "$batch" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid batch' >&2; exit 2; }
binary=${SDDFV_BINARY:-./short_duration_digital_fair_value}
export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_CHECKPOINT_SECS="$checkpoint"
export SHORT_DURATION_DIGITAL_FAIR_VALUE_SYSTEM_LOG_FILE="logs/short-duration-digital-fair-value/${batch}-5m-${checkpoint}s/system.log"
export SHORT_DURATION_DIGITAL_FAIR_VALUE_LOG_FILE="$SHORT_DURATION_DIGITAL_FAIR_VALUE_SYSTEM_LOG_FILE"
export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE="results/short-duration-digital-fair-value/${batch}-5m-${checkpoint}s/events.ndjson"
export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_REPORT_FILE="results/short-duration-digital-fair-value/${batch}-5m-${checkpoint}s/report.json"
export SHORT_DURATION_DIGITAL_FAIR_VALUE_CALIBRATION_SPLIT_REPORT_FILE="results/short-duration-digital-fair-value/${batch}-5m-${checkpoint}s/calibration-split-report.json"
if [[ "$command" == calibrate ]]; then
    exec "$binary" calibrate --refresh
elif [[ "$command" == validate ]]; then
    if [[ "${SDDFV_VALIDATE_REFRESH:-0}" == 1 ]]; then
        exec "$binary" validate --refresh
    fi
    exec "$binary" validate
else
    exec "$binary" "$command"
fi
