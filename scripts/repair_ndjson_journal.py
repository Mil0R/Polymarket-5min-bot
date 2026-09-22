#!/usr/bin/env python3
"""Validate or repair NDJSON journals without silently discarding source bytes.

Default mode is read-only.  --output writes a repaired copy.  --in-place first
creates a timestamped byte-for-byte backup and then atomically replaces the
input.  Stop the writer process before using --in-place.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import tempfile


ACTION_MARKER = '{"action":'


def recover_line(line: str) -> tuple[str | None, bool]:
    try:
        json.loads(line)
        return line, False
    except json.JSONDecodeError:
        pass
    offset = line.find(ACTION_MARKER, 1)
    while offset >= 0:
        suffix = line[offset:]
        try:
            json.loads(suffix)
            return suffix, True
        except json.JSONDecodeError:
            offset = line.find(ACTION_MARKER, offset + 1)
    return None, False


def inspect(path: Path) -> tuple[list[str], list[tuple[int, str]], int]:
    repaired: list[str] = []
    invalid: list[tuple[int, str]] = []
    recovered = 0
    with path.open(encoding="utf-8") as source:
        for line_number, raw in enumerate(source, 1):
            line = raw.rstrip("\n")
            if not line.strip():
                continue
            valid, was_recovered = recover_line(line)
            if valid is None:
                invalid.append((line_number, line))
                continue
            if was_recovered:
                invalid.append((line_number, line))
                recovered += 1
            repaired.append(valid)
    return repaired, invalid, recovered


def write_new(path: Path, lines: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x", encoding="utf-8") as destination:
        for line in lines:
            destination.write(line)
            destination.write("\n")
        destination.flush()
        os.fsync(destination.fileno())


def replace_in_place(path: Path, lines: list[str]) -> Path:
    stamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
    backup = Path(f"{path}.bak-{stamp}")
    backup.write_bytes(path.read_bytes())
    with backup.open("rb") as saved:
        os.fsync(saved.fileno())
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.repair-", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as destination:
            for line in lines:
                destination.write(line)
                destination.write("\n")
            destination.flush()
            os.fsync(destination.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)
    return backup


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("journal", type=Path)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--output", type=Path, help="write a repaired copy; destination must not exist")
    mode.add_argument("--in-place", action="store_true", help="backup and atomically repair the input")
    args = parser.parse_args()

    repaired, invalid, recovered = inspect(args.journal)
    print(
        f"file={args.journal} valid_output_lines={len(repaired)} "
        f"invalid_source_lines={len(invalid)} recovered_suffix_events={recovered} "
        f"dropped_lines={len(invalid) - recovered}"
    )
    for line_number, _ in invalid:
        print(f"invalid_line={line_number}")

    if args.output:
        write_new(args.output, repaired)
        print(f"repaired_copy={args.output}")
    elif args.in_place:
        backup = replace_in_place(args.journal, repaired)
        print(f"backup={backup}")
        print(f"repaired_in_place={args.journal}")
    else:
        print("read_only=true")
    return 0 if not invalid or args.output or args.in_place else 1


if __name__ == "__main__":
    raise SystemExit(main())
