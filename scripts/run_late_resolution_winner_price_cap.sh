#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
action=${1:-dry-run}
batch=${2:-server-low-latency-007}
[[ "$batch" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid batch' >&2; exit 2; }
binary=${LATE_WINNER_BINARY:-./target/release/late_resolution_winner_trading}
result_dir="results/late-resolution-winner-trading/${batch}-5m"
log_dir="logs/${batch}-5m"

case "$action" in
    check)
        "$binary" price-cap-info
        exec "$binary" operator-info
        ;;
    tui)
        exec "$binary" tui --journal "$result_dir/price-cap-study.ndjson" "${@:3}"
        ;;
    operator-report)
        exec python3 scripts/late_resolution_winner_operator_report.py \
            --input "$result_dir/operator-paper.ndjson"
        ;;
    report)
        exec python3 scripts/late_resolution_winner_price_cap_report.py \
            --input "$result_dir/price-cap-study.ndjson"
        ;;
    dry-run)
        # Old binaries silently ignore new env names; require the new capability.
        "$binary" price-cap-info >/dev/null
        "$binary" operator-info >/dev/null
        [[ -f .env ]] || { echo '.env is required for read-only market APIs' >&2; exit 1; }
        mkdir -p "$result_dir" "$log_dir"
        # Preserve 006 and the original unfiltered collector. Only the independent
        # candidate arm has ETH<=.97; BTC and the control still see .96-.98.
        exec env \
            DRY_RUN=true \
            MARKET_PERIOD_MINUTES=5 \
            CRYPTO_SYMBOLS=btc,eth \
            LATE_RESOLUTION_WINNER_TRADING_BEFORE_END_SECS=60 \
            LATE_RESOLUTION_WINNER_TRADING_MIN_PRICE=0.96 \
            LATE_RESOLUTION_WINNER_TRADING_MAX_PRICE=0.98 \
            LATE_RESOLUTION_WINNER_TRADING_MAX_ORDER_USDC=5.0 \
            LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS=2 \
            LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_ENABLED=true \
            LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_FILE="$result_dir/price-cap-study.ndjson" \
            LATE_RESOLUTION_WINNER_TRADING_STATE_JOURNAL_FILE="$result_dir/orders.ndjson" \
            LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE="$result_dir/evaluations.ndjson" \
            LATE_RESOLUTION_WINNER_TRADING_SYSTEM_LOG_FILE="$log_dir/system.log" \
            LATE_RESOLUTION_WINNER_TRADING_STRATEGY_LOG_FILE="$log_dir/late-resolution-winner-trading.log" \
            "$binary" dry-run
        ;;
    *)
        echo 'usage: run_late_resolution_winner_price_cap.sh [dry-run|report|operator-report|check|tui] [batch] [tui-options]' >&2
        exit 2
        ;;
esac
