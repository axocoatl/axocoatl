#!/usr/bin/env python3
"""Knock out a near-uniform / checker-pattern background to transparency
with proper edge de-contamination so the result renders halo-free.

Works for any source whose "background" is reachable from the four
corners via low-saturation pixels — covers white, near-white, and the
checker pattern that some image-gen models draw when prompted with
"transparent background".

Reads:  <src> (RGB or RGBA)
Writes: <out> (RGBA)

Algorithm mirrors tools/wordmark.py edge decon:
  1. Tag near-bg pixels (low saturation OR very light luminance).
  2. Flood from corners through near-bg → exterior mask.
  3. Edge band = morphological ring around the figure silhouette.
  4. Edge α from how non-bg each pixel is; un-blend bg from RGB so the
     edge composites correctly on any backdrop.
  5. Interior figure pixels keep source RGB byte-identical.

Usage:
    python tools/knockout_bg.py SRC OUT
"""
from __future__ import annotations

import sys
from collections import deque
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage as ndi


# Saturation threshold below which a pixel is treated as "background-ish"
# (gray scale, both checker shades qualify; rich figure colors do not).
SAT_MAX = 28
# Anti-alias band around the silhouette where matting cleans up.
BAND_OUTSIDE_PX = 4
BAND_INSIDE_PX = 1


def flood_from_corners(mask: np.ndarray) -> np.ndarray:
    h, w = mask.shape
    visited = np.zeros_like(mask)
    q: deque[tuple[int, int]] = deque()
    for sy, sx in ((0, 0), (0, w - 1), (h - 1, 0), (h - 1, w - 1)):
        if mask[sy, sx]:
            visited[sy, sx] = True
            q.append((sy, sx))
    while q:
        y, x = q.popleft()
        for dy, dx in ((-1, 0), (1, 0), (0, -1), (0, 1)):
            ny, nx = y + dy, x + dx
            if 0 <= ny < h and 0 <= nx < w and not visited[ny, nx] and mask[ny, nx]:
                visited[ny, nx] = True
                q.append((ny, nx))
    return visited


def main() -> int:
    if len(sys.argv) < 3:
        sys.exit("usage: knockout_bg.py SRC OUT")
    src = Path(sys.argv[1])
    out = Path(sys.argv[2])

    rgb = np.array(Image.open(src).convert("RGB"))
    h, w = rgb.shape[:2]
    R = rgb[:, :, 0].astype(int)
    G = rgb[:, :, 1].astype(int)
    B = rgb[:, :, 2].astype(int)
    sat = rgb.max(axis=2).astype(int) - rgb.min(axis=2).astype(int)

    # Background = low saturation (gray-ish checker / white).
    near_bg = sat <= SAT_MAX
    exterior = flood_from_corners(near_bg)
    figure = ~exterior

    # Morphological band.
    fd = ndi.binary_dilation(figure, iterations=BAND_OUTSIDE_PX)
    fe = ndi.binary_erosion(figure, iterations=BAND_INSIDE_PX)
    band = fd & ~fe
    pure_exterior = ~fd

    # Reference background color = mean of pure exterior.
    if pure_exterior.any():
        Kr = float(R[pure_exterior].mean())
        Kg = float(G[pure_exterior].mean())
        Kb = float(B[pure_exterior].mean())
    else:
        Kr = Kg = Kb = 255.0
    print(f"  exterior reference: ({Kr:.0f}, {Kg:.0f}, {Kb:.0f})")

    out_rgb = rgb.copy()
    alpha = np.full((h, w), 255, dtype=np.uint8)

    # Band: α from saturation, decon with bg color.
    band_alpha = np.clip(sat.astype(np.float32) / 100.0, 0.0, 1.0)
    a3 = band_alpha[:, :, None]
    safe = np.clip(a3, 0.04, 1.0)
    K = np.array([[[Kr, Kg, Kb]]], dtype=np.float32)
    decon = (rgb.astype(np.float32) - K * (1.0 - safe)) / safe
    decon = np.clip(decon, 0, 255)

    out_rgb[band] = decon[band].astype(np.uint8)
    alpha[band] = (band_alpha[band] * 255.0).astype(np.uint8)
    alpha[pure_exterior] = 0

    rgba = np.dstack([out_rgb, alpha])
    Image.fromarray(rgba, mode="RGBA").save(out)
    print(
        f"  wrote {out}  opaque={int((alpha == 255).sum()):,}  "
        f"band={int(band.sum()):,}  transparent={int((alpha == 0).sum()):,}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
