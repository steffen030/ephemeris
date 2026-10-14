#!/usr/bin/env python3
"""Parse EPHEMERIS_INK_TRACE logs and summarise sample→present latency.

Usage:
  EPHEMERIS_INK_TRACE=1 ephemeris 2>&1 | tee /tmp/ink.log
  # scribble for ~60s, then:
  python3 scripts/ink-latency-report.py /tmp/ink.log

Accepts a file path or stdin. Looks for structured lines:

  ink_trace stage=present t_us=… sample_to_present_us=…
  ink_trace stats n=… p50_us=… p95_us=… max_us=…
  ink_trace stage=present_blit … blit_us=…
  ink_trace stage=drained|feed_done … age_us=…
"""

from __future__ import annotations

import argparse
import re
import sys
from typing import Iterable, List, Optional


KV_RE = re.compile(r"(\w+)=(\S+)")
INK_RE = re.compile(r"ink_trace\b(.*)$")


def parse_kv(tail: str) -> dict[str, str]:
    return {m.group(1): m.group(2) for m in KV_RE.finditer(tail)}


def percentile(sorted_vals: List[int], p: float) -> int:
    if not sorted_vals:
        return 0
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    idx = int(round((p / 100.0) * (len(sorted_vals) - 1)))
    idx = max(0, min(len(sorted_vals) - 1, idx))
    return sorted_vals[idx]


def summarise(name: str, values: List[int]) -> None:
    if not values:
        print(f"{name}: (no samples)")
        return
    s = sorted(values)
    print(
        f"{name}: n={len(s)}  p50={percentile(s, 50)}µs  "
        f"p95={percentile(s, 95)}µs  max={s[-1]}µs  "
        f"({percentile(s, 50)/1000:.1f} / {percentile(s, 95)/1000:.1f} / {s[-1]/1000:.1f} ms)"
    )


def iter_lines(path: Optional[str]) -> Iterable[str]:
    if path:
        with open(path, encoding="utf-8", errors="replace") as f:
            yield from f
    else:
        yield from sys.stdin


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "log",
        nargs="?",
        help="Path to log file (default: stdin)",
    )
    args = ap.parse_args()

    sample_to_present: List[int] = []
    blit: List[int] = []
    drained_age: List[int] = []
    feed_age: List[int] = []
    embedded_stats: List[dict[str, str]] = []

    for line in iter_lines(args.log):
        m = INK_RE.search(line)
        if not m:
            continue
        kv = parse_kv(m.group(1))
        stage = kv.get("stage")
        if stage == "present" and "sample_to_present_us" in kv:
            sample_to_present.append(int(kv["sample_to_present_us"]))
        elif stage == "present_blit" and "blit_us" in kv:
            blit.append(int(kv["blit_us"]))
        elif stage == "drained" and "age_us" in kv:
            drained_age.append(int(kv["age_us"]))
        elif stage == "feed_done" and "age_us" in kv:
            feed_age.append(int(kv["age_us"]))
        elif "p50_us" in kv and "p95_us" in kv:
            embedded_stats.append(kv)

    print("=== ink latency report (EPHEMERIS_INK_TRACE) ===")
    summarise("sample→present", sample_to_present)
    summarise("present blit", blit)
    summarise("kernel→drained", drained_age)
    summarise("kernel→feed_done", feed_age)

    if embedded_stats:
        print("\n--- in-run stats lines (every ~2s while drawing) ---")
        for s in embedded_stats:
            print(
                f"  n={s.get('n')}  p50={s.get('p50_us')}µs  "
                f"p95={s.get('p95_us')}µs  max={s.get('max_us')}µs  "
                f"window_ms={s.get('window_ms', '?')}"
            )

    if sample_to_present:
        p95 = percentile(sorted(sample_to_present), 95)
        mx = max(sample_to_present)
        ok_p95 = p95 <= 20_000
        ok_max = mx <= 50_000
        print(
            "\nAcceptance (device): sample→present p95≤20ms, max≤50ms → "
            f"p95={'OK' if ok_p95 else 'FAIL'} ({p95/1000:.1f}ms)  "
            f"max={'OK' if ok_max else 'FAIL'} ({mx/1000:.1f}ms)"
        )
        print(
            "Note: off-device landings leave device measurement as TODO in "
            "bead ephemeris-ktg.7 notes."
        )
    elif not any([blit, drained_age, feed_age, embedded_stats]):
        print("No ink_trace lines found.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
