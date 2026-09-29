#!/usr/bin/env bash
set -euo pipefail
# Archive an append-only strategy journal into an immutable, host-tagged segment copy.
#
# Why: journals are opened with O_APPEND, but a same-named file can still be destroyed by a
# `rsync` pull from another host whose copy of that path is shorter or from a different run.
# Copying each segment to a distinct file name makes the data survive name collisions.
#
# Usage:
#   scripts/archive_event_journal.sh status <journal-path>
#   scripts/archive_event_journal.sh archive <journal-path> [label]
#
# Never moves or truncates the source file; archive writes only into <dir>/archive/.

usage() {
    sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

action=${1:-}
journal=${2:-}
label=${3:-$(hostname -s 2>/dev/null || echo host)}

[[ -n "$action" && -n "$journal" ]] || usage
[[ -f "$journal" ]] || { echo "journal not found: $journal" >&2; exit 2; }
dir=$(cd "$(dirname "$journal")" && pwd)
base=$(basename "$journal")
stem=${base%.ndjson}

describe() {
    python3 - "$1" <<'PY'
import json, sys
path = sys.argv[1]
runs, first, last, n, malformed = 0, None, None, 0, 0
with open(path, 'r', encoding='utf-8', errors='replace') as handle:
    for line in handle:
        line = line.strip()
        if not line:
            continue
        n += 1
        try:
            event = json.loads(line)
        except Exception:
            malformed += 1
            continue
        stamp = event.get('recorded_at')
        if isinstance(stamp, str):
            first = stamp if first is None or stamp < first else first
            last = stamp if last is None or stamp > last else last
        if event.get('event') == 'run_started':
            runs += 1
print(f"events={n} malformed={malformed} run_started={runs} first={first} last={last}")
PY
}

case "$action" in
    status)
        printf 'journal  %s\n' "$journal"
        printf 'size     %s bytes\n' "$(stat -c %s "$journal")"
        printf 'sha256   %s\n' "$(sha256sum "$journal" | awk '{print $1}')"
        printf 'content  %s\n' "$(describe "$journal")"
        printf 'archive  %s\n' "$(ls -1 "$dir/archive/${stem}-"*.ndjson 2>/dev/null | wc -l) segment file(s)"
        ls -1 "$dir/archive/${stem}-"*.ndjson 2>/dev/null | sed 's/^/         /' || true
        ;;
    archive)
        stamp=$(date -u +%Y%m%dT%H%M%SZ)
        safe_label=$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '_')
        mkdir -p "$dir/archive"
        target="$dir/archive/${stem}-${stamp}-${safe_label}.ndjson"
        [[ -e "$target" ]] && { echo "refusing to overwrite $target" >&2; exit 3; }
        size_before=$(stat -c %s "$journal")
        sha_before=$(sha256sum "$journal" | awk '{print $1}')
        # Copy, then verify byte-identical: a torn live append can otherwise hide a bad copy.
        cp --no-clobber "$journal" "$target"
        size_after=$(stat -c %s "$target")
        sha_after=$(sha256sum "$target" | awk '{print $1}')
        if [[ "$size_before" != "$size_after" || "$sha_before" != "$sha_after" ]]; then
            echo "archive verification failed: source grew during copy, retry after the writer stops" >&2
            echo "  source=$size_before/$sha_before archive=$size_after/$sha_after" >&2
            exit 4
        fi
        index="$dir/archive/index.ndjson"
        python3 - "$index" "$target" "$sha_after" "$size_after" "$label" "$(describe "$target")" <<'PY'
import json, sys, datetime
index, target, sha, size, label, content = sys.argv[1:7]
row = {
    "archived_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "file": target,
    "sha256": sha,
    "size_bytes": int(size),
    "label": label,
    "content": content,
}
with open(index, 'a', encoding='utf-8') as handle:
    handle.write(json.dumps(row, ensure_ascii=False) + "\n")
PY
        echo "archived $target"
        echo "  sha256 $sha_after  size $size_after  $label"
        ;;
    *)
        usage
        ;;
esac
