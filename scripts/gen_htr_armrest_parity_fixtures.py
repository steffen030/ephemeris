#!/usr/bin/env python3
"""Generate armrest spline parity fixtures for ephemeris-core tests.

Pure-Python mirror of crates/ephemeris-core/src/stroke_htr/armrest.rs
(ported from bkirwi/armrest Apache-2.0). Writes:

  crates/ephemeris-core/tests/fixtures/htr-armrest-parity/
    {name}.ink.txt
    {name}.expected_features.json

Optional: pass path to armrest data/inks/jabberwocky.txt as argv[1].
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "crates/ephemeris-core/tests/fixtures/htr-armrest-parity"

NORMALIZE_HEIGHT = 1.0
SMOOTH_HALF_LIFE = 0.01
MIN_DISTANCE = 0.05
DOUGLAS_PEUCKER = 0.01


class Ink:
    def __init__(self) -> None:
        self.points: list[list[float]] = []
        self.stroke_ends: list[int] = []

    def push(self, x: float, y: float, t: float) -> None:
        self.points.append([x, y, t])

    def pen_up(self) -> None:
        n = len(self.points)
        if n > 0 and (not self.stroke_ends or self.stroke_ends[-1] != n):
            self.stroke_ends.append(n)

    def is_pen_up(self, index: int) -> bool:
        return (index + 1) in self.stroke_ends

    def strokes(self):
        start = 0
        for end in self.stroke_ends:
            yield self.points[start:end]
            start = end

    def clone(self) -> "Ink":
        o = Ink()
        o.points = [p[:] for p in self.points]
        o.stroke_ends = list(self.stroke_ends)
        return o


def from_string(s: str) -> Ink:
    ink = Ink()
    if not s:
        return ink
    for stroke in s.split(";"):
        if not stroke:
            continue
        for point in stroke.split(","):
            parts = [p for p in point.split(" ") if p]
            ink.push(float(parts[0]), float(parts[1]), float(parts[2]))
        ink.pen_up()
    return ink


def ranges(ink: Ink):
    xs = [p[0] for p in ink.points]
    ys = [p[1] for p in ink.points]
    ts = [p[2] for p in ink.points]
    return min(xs), max(xs), min(ys), max(ys), min(ts), max(ts)


def normalize(ink: Ink, target_height: float) -> None:
    xmin, _xmax, ymin, ymax, tmin, _tmax = ranges(ink)
    y_size = ymax - ymin
    scale = target_height / y_size if y_size > 1e-12 else 1.0
    for p in ink.points:
        p[0] = (p[0] - xmin) * scale
        p[1] = (p[1] - ymin) * scale
        p[2] = p[2] - tmin


def smooth(ink: Ink, half_life: float) -> None:
    span = 3
    ends = list(ink.stroke_ends)
    start = 0
    for end in ends:
        stroke = [p[:] for p in ink.points[start:end]]
        n = len(stroke)
        for i in range(n):
            current_range = min(i, span, n - 1 - i)
            current_t = stroke[i][2]
            x = y = tw = 0.0
            for p in stroke[i - current_range : i + current_range + 1]:
                w = 0.5 ** (abs(p[2] - current_t) / half_life)
                x += p[0] * w
                y += p[1] * w
                tw += w
            ink.points[start + i][0] = x / tw
            ink.points[start + i][1] = y / tw
        start = end


def xy_d2(a, b) -> float:
    return (a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2


def point_segment_d2(p0, p1, q) -> float:
    ux, uy = p1[0] - p0[0], p1[1] - p0[1]
    if ux == 0 and uy == 0:
        return xy_d2(p0, q)
    vx, vy = q[0] - p0[0], q[1] - p0[1]
    mag2 = ux * ux + uy * uy
    t = (ux * vx + uy * vy) / mag2
    if t <= 0:
        return xy_d2(p0, q)
    if t >= 1:
        return xy_d2(p1, q)
    px, py = p0[0] + ux * t, p0[1] + uy * t
    return (px - q[0]) ** 2 + (py - q[1]) ** 2


def min_distance(data: Ink, distance: float) -> Ink:
    d2 = distance * distance
    result = Ink()
    for stroke in data.strokes():
        if not stroke:
            result.pen_up()
            continue
        last_kept = stroke[0]
        result.push(*last_kept)
        for nxt in stroke[1:]:
            if xy_d2(last_kept, nxt) >= d2:
                result.push(*nxt)
                last_kept = nxt
        last = stroke[-1]
        if last != last_kept:
            result.push(*last)
        result.pen_up()
    return result


def douglas_peucker(data: Ink, distance: float) -> Ink:
    d2 = distance * distance
    result = Ink()
    for stroke in data.strokes():
        if not stroke:
            result.pen_up()
            continue
        if len(stroke) == 1:
            result.push(*stroke[0])
            result.pen_up()
            continue
        stack = [(0, len(stroke) - 1)]
        while stack:
            start, end = stack.pop()
            p0, p1 = stroke[start], stroke[end]
            inner = None
            for i in range(start + 1, end):
                d = point_segment_d2(p0, p1, stroke[i])
                if inner is None or d > inner[1]:
                    inner = (i, d)
            if inner is not None and inner[1] > d2:
                split = inner[0]
                stack.append((split, end))
                stack.append((start, split))
            else:
                result.push(*stroke[start])
        result.push(*stroke[-1])
        result.pen_up()
    return result


def featurize(s: str) -> list[list[float]]:
    ink = from_string(s)
    if not ink.points:
        return []
    normal = ink.clone()
    normalize(normal, NORMALIZE_HEIGHT)
    smooth(normal, SMOOTH_HALF_LIFE)
    normal = min_distance(normal, MIN_DISTANCE)
    normal = douglas_peucker(normal, DOUGLAS_PEUCKER)
    out: list[list[float]] = []
    last = normal.points[0]
    for i, point in enumerate(normal.points):
        out.append(
            [
                point[0] - last[0],
                point[1] - last[1],
                point[2] - last[2],
                1.0 if normal.is_pen_up(i) else 0.0,
            ]
        )
        last = point
    return out


def main() -> None:
    synth = (
        "0.0 0.0 0.0,0.1 0.5 0.1,0.2 1.0 0.2,0.3 0.4 0.3;"
        "0.4 0.2 0.5,0.6 0.9 0.6,0.8 0.1 0.7"
    )
    fixtures = {"synth-two-stroke": synth}

    jabber = Path(sys.argv[1]) if len(sys.argv) > 1 else None
    if jabber and jabber.is_file():
        for line in jabber.read_text().splitlines():
            if line.startswith("And,\t"):
                fixtures["jabberwocky-and"] = line.split("\t", 1)[1].strip()
                break

    OUT.mkdir(parents=True, exist_ok=True)
    for name, ink in fixtures.items():
        feats = featurize(ink)
        (OUT / f"{name}.ink.txt").write_text(ink + "\n")
        payload = {"features": [[round(v, 6) for v in row] for row in feats]}
        (OUT / f"{name}.expected_features.json").write_text(
            json.dumps(payload, indent=2) + "\n"
        )
        print(f"{name}: T={len(feats)}")


if __name__ == "__main__":
    main()
