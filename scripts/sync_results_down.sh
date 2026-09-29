#!/usr/bin/env bash
set -euo pipefail
# Pull strategy results/logs from the remote host WITHOUT ever overwriting a local file.
#
# Incident this prevents (2026-09-18): the local copy of
# results/short-duration-digital-fair-value/server-low-latency-008-5m-30s/events.ndjson held the
# only copy of segment 1 (2026-09-14 -> 09-15, 1032 observations). A plain
#   rsync -azP remote:.../results/ results/
# replaced it with the remote file, which only contained segment 2 (started 09-18).
# rsync has no notion of "keep the more complete copy"; it overwrites on name.
#
# Usage:
#   scripts/sync_results_down.sh dry-run [scope]   # show what would change, touch nothing
#   scripts/sync_results_down.sh fetch   [scope]   # land a copy under results-incoming/<UTC stamp>/
#   scripts/sync_results_down.sh merge   [staged]  # copy into the live tree, --ignore-existing
#
# 'scope' is a path relative to results/ and logs/, e.g. short-duration-digital-fair-value/
# Env: REMOTE (default aws-eu), REMOTE_ROOT (default /home/ubuntu/poly), STAGING

REMOTE=${REMOTE:-aws-eu}
REMOTE_ROOT=${REMOTE_ROOT:-/home/ubuntu/poly}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
STAGING=${STAGING:-results-incoming/$STAMP}

usage() {
    sed -n '3,17p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

pull() {
    local mode=$1
    local scope=${2:-}
    local extra=()
    [[ "$mode" == dry-run ]] && extra+=(--dry-run)
    rsync -az --itemize-changes "${extra[@]}" \
        --exclude '*.lock' --exclude '*-shm' --exclude '*-wal' \
        "$REMOTE:$REMOTE_ROOT/results/$scope" "$STAGING/results/$scope"
    rsync -az --itemize-changes "${extra[@]}" \
        --exclude '*.lock' --exclude '*-shm' --exclude '*-wal' \
        "$REMOTE:$REMOTE_ROOT/logs/$scope" "$STAGING/logs/$scope"
}

action=${1:-}
scope=${2:-}
case "$action" in
    dry-run)
        echo "== rsync dry-run against $REMOTE:$REMOTE_ROOT${scope:+ (scope $scope)} (nothing will be written)"
        pull dry-run "$scope"
        echo
        echo "Lines starting with '>f' are remote->local transfers."
        echo "  '>f+++++++++'                = destination did not exist (nothing lost)"
        echo "  '>f.st......' / '>fcst.....' = an existing file WOULD BE REPLACED"
        echo "Run 'fetch' to land a copy under $STAGING instead of touching the live tree."
        ;;
    fetch)
        mkdir -p "$STAGING/results/$scope" "$STAGING/logs/$scope"
        pull fetch "$scope"
        echo
        echo "staged under: $STAGING"
        echo "live tree untouched. Inspect, then 'merge' if you want it in place."
        if [[ -n "$(find "$STAGING" -type f -newermt '-1 day' 2>/dev/null | head -1)" ]]; then
            echo "note: remote files modified in the last 24h:"
            find "$STAGING" -type f -newermt '-1 day' -printf '  %s %TY-%Tm-%Td %TH:%TM %p\n' | head -20
        fi
        ;;
    merge)
        latest=$(ls -1d results-incoming/*/ 2>/dev/null | sort | tail -1 || true)
        [[ -n "$latest" ]] || { echo "no results-incoming/<stamp>/ staging dir found; run 'fetch' first" >&2; exit 2; }
        incoming=${scope:-$latest}
        echo "== merging $incoming into the live tree (append-only supersets are updated, others are kept)"
        python3 - "$incoming" "$PWD" <<'PY'
import filecmp, os, shutil, sys, datetime

incoming, live = sys.argv[1], sys.argv[2]
stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
added = updated = kept = 0

def is_strict_prefix(short, long):
    with open(short, 'rb') as a, open(long, 'rb') as b:
        size = os.path.getsize(short)
        while True:
            chunk = a.read(1 << 20)
            if not chunk:
                return True
            if b.read(len(chunk)) != chunk:
                return False
            size -= len(chunk)
            if size <= 0:
                return True

for top in ('results', 'logs'):
    root = os.path.join(incoming, top)
    if not os.path.isdir(root):
        continue
    for dirpath, _, files in os.walk(root):
        for name in files:
            src = os.path.join(dirpath, name)
            rel = os.path.relpath(src, incoming)
            dst = os.path.join(live, rel)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if not os.path.exists(dst):
                shutil.copy2(src, dst)
                print(f"  added    {rel}")
                added += 1
                continue
            if filecmp.cmp(src, dst, shallow=False):
                kept += 1
                continue
            if is_strict_prefix(dst, src):
                backup = f"{dst}.bak-{stamp}"
                shutil.copy2(dst, backup)
                shutil.copy2(src, dst)
                print(f"  updated  {rel}  (local was a strict prefix; previous copy kept at {os.path.basename(backup)})")
                updated += 1
            else:
                print(f"  KEPT     {rel}  WARNING: local and remote diverged; not replaced")
                kept += 1
print(f"added={added} updated={updated} kept/identical={kept}")
PY
        echo
        echo "Diverged files were left untouched. If the local copy is the only source of a segment,"
        echo "archive it before any manual overwrite:"
        echo "  scripts/archive_event_journal.sh archive <journal-path> <host-or-label>"
        ;;
    *)
        usage
        ;;
esac
