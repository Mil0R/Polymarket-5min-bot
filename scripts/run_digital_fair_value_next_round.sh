#!/usr/bin/env bash
set -euo pipefail
# Each stage must pass its checks; observe is explicitly passive data collection.
project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
action=${1:-study}
batch=${2:-market-offset-011}
[[ "$batch" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid batch' >&2; exit 2; }
model_dir=${SDDFV_NEXT_MODEL_DIR:-results/short-duration-digital-fair-value/market-offset-009-development-v2}
binary=${SDDFV_BINARY:-./target/release/short_duration_digital_fair_value}
case "$action" in
    study)
        exec python3 scripts/digital_fair_value_next_round.py study --output "$model_dir" --input \
            results/short-duration-digital-fair-value/market-residual-execution-007-5m-30s/events.ndjson \
            results/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/events.ndjson
        ;;
    parity)
        sha=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["model_sha256"])' "$model_dir/report.json")
        exec "$binary" terminal-parity "$model_dir/model.json" "$sha" "$model_dir/inference-fixture.ndjson" "$model_dir/parity-report.json"
        ;;
    freeze)
        exec python3 scripts/digital_fair_value_next_round.py freeze --output "$model_dir"
        ;;
    start|observe)
        command -v timeout >/dev/null || { echo 'GNU timeout required' >&2; exit 2; }
        [[ -x "$binary" ]] || { echo "binary not executable: $binary" >&2; exit 2; }
        settings=$(python3 scripts/digital_fair_value_next_round.py preflight --output "$model_dir" \
            --journal "results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson" --mode "$action")
        readarray -t values <<< "$settings"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_TERMINAL_MODEL_FILE="$model_dir/model.json"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_TERMINAL_MODEL_SHA256="${values[0]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_SYMBOLS=btc,eth
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_BUDGET_USDC="${values[2]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_EXPOSURE_USDC="${values[3]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_ASSET_EXPOSURE_USDC="${values[4]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_ORDER_USDC="${values[5]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_SLIPPAGE_BPS="${values[6]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_LIQUIDITY_HAIRCUT="${values[7]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_ORDER_FAILURE_RATE="${values[8]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_NET_EDGE="${values[9]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_PRICE="${values[10]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_PRICE="${values[11]}"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_REFERENCE_BUCKETS=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_AUDIT_MARKET_FEES=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_FILE=''
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_SHA256=''
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_OBSERVATION_ONLY=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_EXECUTION_PROBE=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_CAPTURE_RAW_REFERENCE=false
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS=0
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_EXECUTION_DELAY_MS=1000
        echo "SDDFV $action: BTC/ETH read-only; batch=$batch; deadline remains fixed across restarts"
        exec timeout --preserve-status --signal=INT --kill-after=30s "${values[1]}s" \
            env SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" dry-run
        ;;
    report|calibrate)
        exec env SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" "$action"
        ;;
    validate)
        output=${SDDFV_NEXT_VALIDATION_OUTPUT:-results/short-duration-digital-fair-value/${batch}-validation-$(date -u +%Y%m%dT%H%M%SZ)}
        python3 scripts/digital_fair_value_terminal_study.py \
            --input "results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson" \
            --model "$model_dir/model.json" --symbols btc,eth --output "$output"
        exec python3 scripts/digital_fair_value_next_round.py audit --output "$output" \
            --input "results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson"
        ;;
    *) echo 'usage: run_digital_fair_value_next_round.sh [study|parity|freeze|start|observe|report|calibrate|validate] [NEW-batch]' >&2; exit 2 ;;
esac
