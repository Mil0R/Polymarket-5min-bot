#!/usr/bin/env bash
set -euo pipefail

duration_seconds="${1:-1800}"
if [[ ! "$duration_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: $0 [positive-duration-seconds]" >&2
  exit 2
fi
if [[ ! -f .env ]]; then
  echo ".env is required for authenticated read-only APIs" >&2
  exit 1
fi

echo "Starting read-only dry-run soak for ${duration_seconds}s"
timeout --signal=INT --kill-after=10s "${duration_seconds}s" \
  env \
    DRY_RUN=true \
    CANARY_MODE=false \
    ARBITRAGE_STATE_JOURNAL_FILE=results/binary-complement-arbitrage/paper/orders.ndjson \
    WINNER_BUY_STATE_JOURNAL_FILE=results/late-resolution-winner-trading/paper/orders.ndjson \
    WINNER_BUY_EVALUATION_JOURNAL_FILE=results/late-resolution-winner-trading/paper/evaluations.ndjson \
    ARBITRAGE_LOG_FILE=results/binary-complement-arbitrage/paper/opportunities.ndjson \
    POSITION_SYNC_INTERVAL_SECS=0 \
    POSITION_BALANCE_INTERVAL_SECS=0 \
    MERGE_INTERVAL_MINUTES=0 \
    WIND_DOWN_BEFORE_WINDOW_END_MINUTES=0 \
    TUI_ENABLED=false \
    PLAIN_LOGS=true \
    cargo run
