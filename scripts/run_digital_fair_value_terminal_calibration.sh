#!/usr/bin/env bash
set -euo pipefail

# SDDFV terminal calibration: offline study, inference parity, passive forward audit.
project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
action=${1:-study}
batch=${2:-terminal-calibration-008}
[[ "$batch" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid batch' >&2; exit 2; }
binary=${SDDFV_BINARY:-./target/release/short_duration_digital_fair_value}
model_dir=${SDDFV_TERMINAL_MODEL_DIR:-results/short-duration-digital-fair-value/terminal-calibration-008-study-v3}
case "$action" in
    study)
        exec python3 scripts/digital_fair_value_terminal_study.py \
          --input "${SDDFV_TERMINAL_STUDY_INPUT:-results/short-duration-digital-fair-value/market-residual-execution-007-5m-30s/events.ndjson}" \
          --output "$model_dir"
        ;;
    parity)
        expected_sha=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["model_sha256"])' "$model_dir/report.json")
        exec "$binary" terminal-parity "$model_dir/model.json" "$expected_sha" \
          "$model_dir/inference-fixture.ndjson" "$model_dir/parity-report.json"
        ;;
    validate-frozen)
        exec python3 scripts/digital_fair_value_terminal_study.py \
          --input "results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson" \
          --model "$model_dir/model.json" \
          --output "${SDDFV_TERMINAL_VALIDATION_OUTPUT:-results/short-duration-digital-fair-value/${batch}-fresh-study}"
        ;;
    report|calibrate)
        exec env SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" "$action"
        ;;
    dry-run)
        journal="results/short-duration-digital-fair-value/${batch}-5m-30s/events.ndjson"
        # Trust the completed parity report, not a digest silently recomputed from
        # a potentially replaced model. A changed model requires a new batch.
        expected_sha=$(python3 - "$model_dir" "$journal" <<'PY'
import hashlib,json,sys
from pathlib import Path
root,journal=Path(sys.argv[1]),Path(sys.argv[2])
report=json.loads((root/"parity-report.json").read_text())
digest=hashlib.sha256((root/"model.json").read_bytes()).hexdigest()
if not report.get("passed") or report.get("forecasts",0)==0 or digest!=report["model_sha256"]:
    raise SystemExit("terminal model needs a successful matching parity report")
if journal.exists():
    with journal.open() as stream:
        for line in stream:
            event=json.loads(line)
            if event.get("event")=="run_started" and (event.get("terminal_model") or {}).get("sha256")!=digest:
                raise SystemExit("batch contains a different model; use a NEW batch")
print(digest)
PY
        )
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_TERMINAL_MODEL_FILE="$model_dir/model.json"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_TERMINAL_MODEL_SHA256="$expected_sha"
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_FILE=''
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MODEL_SHA256=''
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_OBSERVATION_ONLY=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_EXECUTION_PROBE=true
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_EXECUTION_DELAY_MS=1000
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_CAPTURE_RAW_REFERENCE=false
        export SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS=0
        exec env SDDFV_BINARY="$binary" bash scripts/run_digital_fair_value_research.sh 30 "$batch" dry-run
        ;;
    *) echo 'usage: run_digital_fair_value_terminal_calibration.sh [study|parity|dry-run|report|calibrate|validate-frozen] [new-batch]' >&2; exit 2 ;;
esac
