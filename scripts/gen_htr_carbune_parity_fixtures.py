#!/usr/bin/env python3
"""Generate Carbune2020 / OnlineHTR golden feature fixtures for ephemeris-core.

Tries numpy+scipy (OnlineHTR parity); falls back to a pure-Python linear
interp that matches scipy for the checked-in two-point strokes.

Usage:
  python3 scripts/gen_htr_carbune_parity_fixtures.py
"""

from __future__ import annotations

import json
import math
from pathlib import Path

POINTS_PER_UNIT = 20.0
OUT_DIR = (
    Path(__file__).resolve().parents[1]
    / "crates"
    / "ephemeris-core"
    / "tests"
    / "fixtures"
    / "htr-carbune-parity"
)

SAMPLES = [
    {
        "name": "two-stroke-short-real-t0",
        "strokes": [
            {
                "t0_ms": 1000,
                "points": [
                    {"x": 0, "y": 0, "t_ms": 0},
                    {"x": 8, "y": 6, "t_ms": 100},
                ],
            },
            {
                "t0_ms": 2000,
                "points": [
                    {"x": 20, "y": 100, "t_ms": 0},
                    {"x": 28, "y": 100, "t_ms": 100},
                ],
            },
        ],
    },
    {
        "name": "two-stroke-short-synth-t0",
        "strokes": [
            {
                "t0_ms": 0,
                "points": [
                    {"x": 0, "y": 0, "t_ms": 0},
                    {"x": 8, "y": 6, "t_ms": 100},
                ],
            },
            {
                "t0_ms": 0,
                "points": [
                    {"x": 20, "y": 100, "t_ms": 0},
                    {"x": 28, "y": 100, "t_ms": 100},
                ],
            },
        ],
    },
]


def strokes_to_points(strokes):
    pts = []
    next_synth = 0.0
    for si, st in enumerate(strokes):
        t0 = float(st.get("t0_ms", 0))
        t0_use = next_synth if t0 == 0 else t0
        last = t0_use
        for p in st["points"]:
            abs_ms = t0_use + float(p["t_ms"])
            last = abs_ms
            pts.append(
                {
                    "x": float(p["x"]),
                    "y": float(p["y"]),
                    "t": abs_ms / 1000.0,
                    "stroke_nr": si,
                }
            )
        next_synth = last + 150.0
    return pts


def _linspace(a, b, n):
    if n == 1:
        return [a]
    return [a + (b - a) * i / (n - 1) for i in range(n)]


def _interp1d(xs, ys, xq):
    out = []
    for x in xq:
        if x <= xs[0]:
            out.append(ys[0])
            continue
        if x >= xs[-1]:
            out.append(ys[-1])
            continue
        for i in range(len(xs) - 1):
            if xs[i] <= x <= xs[i + 1]:
                d = xs[i + 1] - xs[i]
                out.append(ys[i] if abs(d) < 1e-12 else ys[i] + (x - xs[i]) / d * (ys[i + 1] - ys[i]))
                break
    return out


def featurize_pure(points):
    xs = [p["x"] for p in points]
    ys = [p["y"] for p in points]
    ts = [p["t"] for p in points]
    sn = [p["stroke_nr"] for p in points]
    x0, ymin = xs[0], min(ys)
    xs = [(x - x0) for x in xs]
    ys = [(y - ymin) for y in ys]
    scale = max(ys) or 1.0
    xs = [x / scale for x in xs]
    ys = [y / scale for y in ys]

    strokes = {}
    order = []
    for x, y, t, s in zip(xs, ys, ts, sn):
        if s not in strokes:
            strokes[s] = []
            order.append(s)
        strokes[s].append((x, y, t))

    resampled = []
    for s in order:
        sp = strokes[s]
        if len(sp) == 1:
            resampled.append((*sp[0], s))
            continue
        length = sum(
            math.hypot(sp[i][0] - sp[i - 1][0], sp[i][1] - sp[i - 1][1])
            for i in range(1, len(sp))
        )
        nr = int(math.ceil(length * POINTS_PER_UNIT))
        if nr == 1:
            nr = 2
        dts = [sp[i][2] - sp[i - 1][2] for i in range(1, len(sp))]
        use_time = all(d >= 0 for d in dts) and not all(abs(d) < 1e-12 for d in dts)
        if use_time:
            tmin, tmax = sp[0][2], sp[-1][2]
            param = [(p[2] - tmin) / (tmax - tmin) for p in sp]
        else:
            cum = [0.0]
            for i in range(1, len(sp)):
                cum.append(
                    cum[-1] + math.hypot(sp[i][0] - sp[i - 1][0], sp[i][1] - sp[i - 1][1])
                )
            total = cum[-1] or 1.0
            param = [c / total for c in cum]
        grid = _linspace(param[0], param[-1], nr)
        rx = _interp1d(param, [p[0] for p in sp], grid)
        ry = _interp1d(param, [p[1] for p in sp], grid)
        rt = _interp1d(param, [p[2] for p in sp], grid)
        for x, y, t in zip(rx, ry, rt):
            resampled.append((x, y, t, s))

    feats = [[0.0, 0.0, 0.0, 1.0]]
    for i in range(1, len(resampled)):
        a, b = resampled[i - 1], resampled[i]
        feats.append(
            [
                b[0] - a[0],
                b[1] - a[1],
                b[2] - a[2],
                1.0 if b[3] != a[3] else 0.0,
            ]
        )
    return feats


def featurize_scipy(points):
    import numpy as np
    import pandas as pd
    from scipy.interpolate import interp1d

    df = pd.DataFrame(points)
    df["x"] = df["x"] - df["x"].iloc[0]
    df["y"] = df["y"] - df["y"].min()
    scale = float(df["y"].max()) or 1.0
    df["x"] = df["x"] / scale
    df["y"] = df["y"] / scale

    distances = []
    stroke_nrs = []
    for stroke_nr, g in df.groupby("stroke_nr"):
        dist = np.sqrt(g["x"].diff() ** 2 + g["y"].diff() ** 2)
        for d in dist:
            distances.append(d)
            stroke_nrs.append(stroke_nr)
    stroke_lengths = (
        pd.DataFrame({"distances": distances, "stroke_nr": stroke_nrs})
        .groupby("stroke_nr")
        .sum()
    )
    stroke_lengths["nr"] = np.ceil(stroke_lengths["distances"] * POINTS_PER_UNIT)
    stroke_lengths.loc[stroke_lengths["nr"] == 1.0, "nr"] += 1
    stroke_lengths["nr"] = stroke_lengths["nr"].astype(int)

    data = {"x": [], "y": [], "t": [], "stroke_nr": []}
    for stroke_nr, g in df.groupby("stroke_nr"):
        if g.shape[0] == 1:
            idx = g.index[0]
            data["t"].append(g.loc[idx, "t"])
            data["x"].append(g.loc[idx, "x"])
            data["y"].append(g.loc[idx, "y"])
            data["stroke_nr"].append(stroke_nr)
            continue
        dts = g["t"].diff().iloc[1:]
        if np.allclose(dts, 0) or not np.all(dts >= 0.0):
            # arc-length fallback (production path; OnlineHTR would discard)
            cum = np.concatenate([[0.0], np.cumsum(np.sqrt(g["x"].diff().fillna(0) ** 2 + g["y"].diff().fillna(0) ** 2).iloc[1:].to_numpy())])
            total = cum[-1] if cum[-1] > 0 else 1.0
            param = cum / total
        else:
            param = ((g["t"] - g["t"].min()) / (g["t"].max() - g["t"].min())).to_numpy()
        nr = int(stroke_lengths.loc[stroke_nr, "nr"])
        grid = np.linspace(param.min(), param.max(), nr)
        for ch in ("x", "y", "t"):
            data[ch].extend(interp1d(param, g[ch], kind="linear", bounds_error=True)(grid))
        data["stroke_nr"].extend([stroke_nr] * nr)

    rdf = pd.DataFrame(data)
    feats = [[0.0, 0.0, 0.0, 1.0]]
    for i in range(1, len(rdf)):
        n = 1.0 if rdf.loc[i, "stroke_nr"] != rdf.loc[i - 1, "stroke_nr"] else 0.0
        feats.append(
            [
                float(rdf.loc[i, "x"] - rdf.loc[i - 1, "x"]),
                float(rdf.loc[i, "y"] - rdf.loc[i - 1, "y"]),
                float(rdf.loc[i, "t"] - rdf.loc[i - 1, "t"]),
                n,
            ]
        )
    return feats


def main():
    try:
        import numpy  # noqa: F401
        import scipy  # noqa: F401

        featurize = featurize_scipy
        backend = "numpy+scipy"
    except ImportError:
        featurize = featurize_pure
        backend = "pure-python"

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    print(f"backend={backend} → {OUT_DIR}")
    for sample in SAMPLES:
        pts = strokes_to_points(sample["strokes"])
        feats = featurize(pts)
        # Round for stable JSON
        feats = [[round(v, 6) for v in row] for row in feats]
        ink_path = OUT_DIR / f"{sample['name']}.ink.json"
        feat_path = OUT_DIR / f"{sample['name']}.expected_features.json"
        ink_path.write_text(
            json.dumps(
                {
                    "name": sample["name"],
                    "strokes": sample["strokes"],
                    "absolute_points_seconds": pts,
                },
                indent=2,
            )
            + "\n"
        )
        feat_path.write_text(json.dumps({"features": feats}, indent=2) + "\n")
        print(f"  {sample['name']}: T={len(feats)}")


if __name__ == "__main__":
    main()
