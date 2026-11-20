#!/usr/bin/env python3
"""Compare `[tools] code_first` on and off for one task.

Runs the task N times in each mode with `azazel -p`, one run at a time, and sums what the usage
ledger recorded for each run: requests, fresh input, output, cache reads and writes, estimated
cost, and wall time. The ledger is `<home>/usage/YYYY-MM.jsonl`, one JSON line per model request
with the fields `at`, `model`, `session`, `agent`, `input`, `output`, `cache_read`, `cache_write`,
`cost_usd` (absent for a model without a price) and `requests`.

The runs use your real azazel home and models (they cost what they cost), so run it in a scratch
copy of a project: the task may edit files. Nothing else should use azazel while it runs, because
a run is told apart by the time its requests were answered.

    python tools/code-first-compare.py --runs 3 "find where the retry policy is defined and explain it"
    python tools/code-first-compare.py --runs 5 --model anthropic/claude-sonnet-4-5 --cwd ~/scratch/proj "..."
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def home_dir(args):
    if args.home:
        return Path(args.home)
    if os.environ.get("AZAZEL_HOME"):
        return Path(os.environ["AZAZEL_HOME"])
    return Path.home() / ".azazel"


def parse_time(text):
    # Timestamps look like 2026-11-20T10:11:12.123456789Z; datetime keeps microseconds.
    text = text.rstrip("Z")
    if "." in text:
        head, frac = text.split(".", 1)
        text = f"{head}.{frac[:6]}"
    return datetime.fromisoformat(text).replace(tzinfo=timezone.utc)


def ledger_lines(home, since, until):
    """The ledger lines answered between `since` and `until`."""
    out = []
    usage = home / "usage"
    if not usage.is_dir():
        return out
    for path in sorted(usage.glob("*.jsonl"))[-2:]:  # a run can straddle a month change
        with open(path, encoding="utf-8") as file:
            for raw in file:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    line = json.loads(raw)
                    at = parse_time(line["at"])
                except (ValueError, KeyError):
                    continue
                if since <= at <= until:
                    out.append(line)
    return out


def run_once(args, home, code_first):
    command = [args.azazel, "-p", "--mode", args.mode, "--no-memory",
               "--set", f"tools.code_first={'true' if code_first else 'false'}",
               "--set", "session.auto_name=false"]
    if args.model:
        command += ["--model", args.model]
    if args.cwd:
        command += ["--cwd", args.cwd]
    command.append(args.task)
    since = datetime.now(timezone.utc)
    started = time.monotonic()
    done = subprocess.run(command, capture_output=True, text=True)
    seconds = time.monotonic() - started
    until = datetime.now(timezone.utc)
    lines = ledger_lines(home, since, until)
    total = {
        "requests": sum(line.get("requests", 1) for line in lines),
        "input": sum(line.get("input", 0) for line in lines),
        "output": sum(line.get("output", 0) for line in lines),
        "cache_read": sum(line.get("cache_read", 0) for line in lines),
        "cache_write": sum(line.get("cache_write", 0) for line in lines),
        "cost_usd": sum(line.get("cost_usd") or 0.0 for line in lines),
        "seconds": seconds,
        "ok": done.returncode == 0,
    }
    if done.returncode != 0:
        print(f"  run failed ({done.returncode}): {done.stderr.strip()[-300:]}", file=sys.stderr)
    if not lines:
        print("  no ledger lines for this run: is the usage ledger in " + str(home / "usage") + "?", file=sys.stderr)
    return total


FIELDS = [
    ("requests", "requests", "{:.1f}"),
    ("input", "input tokens", "{:,.0f}"),
    ("output", "output tokens", "{:,.0f}"),
    ("cache_read", "cache read", "{:,.0f}"),
    ("cache_write", "cache write", "{:,.0f}"),
    ("cost_usd", "cost (USD)", "{:.4f}"),
    ("seconds", "time (s)", "{:.1f}"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("task", help="the prompt to run")
    parser.add_argument("--runs", type=int, default=3, help="runs per mode (default 3)")
    parser.add_argument("--model", help="model for both modes, e.g. anthropic/claude-sonnet-4-5")
    parser.add_argument("--cwd", help="project directory to run in")
    parser.add_argument("--mode", default="auto", help="permission mode (default auto)")
    parser.add_argument("--azazel", default=shutil.which("azazel") or "azazel", help="the azazel binary")
    parser.add_argument("--home", help="azazel home (default $AZAZEL_HOME or ~/.azazel)")
    args = parser.parse_args()

    home = home_dir(args)
    results = {False: [], True: []}
    # Alternate the modes so that drift (provider load, cache warmth) hits both alike.
    for n in range(args.runs):
        for code_first in (False, True):
            label = "code-first" if code_first else "standard  "
            print(f"run {n + 1}/{args.runs} {label}", flush=True)
            results[code_first].append(run_once(args, home, code_first))

    print()
    print(f"{'median of ' + str(args.runs):<16}{'standard':>14}{'code-first':>14}{'change':>10}")
    for key, name, fmt in FIELDS:
        base = statistics.median(r[key] for r in results[False])
        code = statistics.median(r[key] for r in results[True])
        change = f"{(code - base) / base * 100:+.0f}%" if base else "n/a"
        print(f"{name:<16}{fmt.format(base):>14}{fmt.format(code):>14}{change:>10}")
    failed = sum(not r["ok"] for mode in results.values() for r in mode)
    if failed:
        print(f"\n{failed} run(s) exited with an error; their numbers are included.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
