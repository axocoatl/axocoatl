#!/usr/bin/env python3
"""Process the AI-generated wordmark — knock out the black background
to transparent, apply edge de-contamination so the wordmark renders
halo-free on any backdrop.

Input:  the promoted generated wordmark (RGB on solid black background)
Output: branding/wordmark.png             — transparent background
        branding/wordmark-ink.png         — wordmark composited on ink #0E1218
        branding/wordmark-vellum.png      — wordmark composited on parchment #F4ECDA

Re-run after promoting any new generated wordmark candidate to the
canonical slot — point SRC at the promoted file.

Algorithm mirrors tools/cut_cpu.py:
  1. Flood-fill near-black from the four corners → exterior.
  2. Edge band around the silhouette → smooth α + un-blend the black.
  3. Interior wordmark pixels kept byte-identical from the source.
"""
from __future__ import annotations

import sys
from collections import deque
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage as ndi


REPO = Path(__file__).resolve().parent.parent
SRC_DEFAULT = REPO / "branding" / "_wordmark-source.png"
OUT_DIR = REPO / "branding"

# Pixel counts as "outside background" if every channel is below this.
NEAR_BLACK_MAX_CH = 50
# Anti-alias band around the silhouette where matting cleans up.
BAND_OUTSIDE_PX = 4
BAND_INSIDE_PX = 1

INK = (14, 18, 24)
PARCHMENT = (244, 236, 218)


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


def knockout_black_bg(rgb: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Return (rgb_decon, alpha).  Inside pixels keep source RGB exactly."""
    h, w = rgb.shape[:2]

    # Pixels that look like background-or-AA-edge: every channel ≤ threshold.
    near_black = (rgb <= NEAR_BLACK_MAX_CH).all(axis=2)
    exterior = flood_from_corners(near_black)
    figure = ~exterior

    figure_dilated = ndi.binary_dilation(figure, iterations=BAND_OUTSIDE_PX)
    figure_eroded = ndi.binary_erosion(figure, iterations=BAND_INSIDE_PX)
    band = figure_dilated & ~figure_eroded
    pure_exterior = ~figure_dilated

    # Mean colour of the pure-black exterior = the colour we un-blend.
    if pure_exterior.any():
        K_r = float(rgb[:, :, 0][pure_exterior].mean())
        K_g = float(rgb[:, :, 1][pure_exterior].mean())
        K_b = float(rgb[:, :, 2][pure_exterior].mean())
    else:
        K_r = K_g = K_b = 0.0
    print(f"  exterior black reference: ({K_r:.0f}, {K_g:.0f}, {K_b:.0f})")

    out_rgb = rgb.copy()
    alpha = np.full((h, w), 255, dtype=np.uint8)

    # α derived from how far each pixel is from background brightness.
    # The brighter / more saturated, the higher α.
    mx = rgb.max(axis=2).astype(np.float32)
    band_alpha = np.clip(mx / 255.0, 0.0, 1.0)
    a3 = band_alpha[:, :, None]
    safe = np.clip(a3, 0.04, 1.0)
    K_arr = np.array([[[K_r, K_g, K_b]]], dtype=np.float32)
    decon = (rgb.astype(np.float32) - K_arr * (1.0 - safe)) / safe
    decon = np.clip(decon, 0, 255)

    out_rgb[band] = decon[band].astype(np.uint8)
    alpha[band] = (band_alpha[band] * 255.0).astype(np.uint8)
    alpha[pure_exterior] = 0
    return out_rgb, alpha


def composite_on(rgba: np.ndarray, bg: tuple[int, int, int]) -> np.ndarray:
    """Flatten the RGBA wordmark over a solid background colour."""
    rgb = rgba[:, :, :3].astype(np.float32)
    a = rgba[:, :, 3:4].astype(np.float32) / 255.0
    bg_arr = np.full_like(rgb, bg, dtype=np.float32)
    return (rgb * a + bg_arr * (1.0 - a)).astype(np.uint8)


def main() -> int:
    src = Path(sys.argv[1]) if len(sys.argv) > 1 else SRC_DEFAULT
    if not src.exists():
        sys.exit(f"source not found: {src}")
    rgb = np.array(Image.open(src).convert("RGB"))
    print(f"source: {src}  ({rgb.shape[1]}×{rgb.shape[0]})")

    print("step 1: knock out black background")
    out_rgb, alpha = knockout_black_bg(rgb)
    rgba = np.dstack([out_rgb, alpha])

    # Crop transparent padding so the wordmark sits tight in its bbox.
    # The AI source is 1024×1024 but the actual wordmark content fills
    # only the central horizontal band — without this, displaying the
    # PNG at a fixed height renders the wordmark much smaller than
    # intended.  Add a tiny margin so anti-aliased edges aren't clipped.
    pil = Image.fromarray(rgba, mode="RGBA")
    bbox = pil.getbbox()
    if bbox:
        x0, y0, x1, y1 = bbox
        margin = 6
        x0 = max(0, x0 - margin)
        y0 = max(0, y0 - margin)
        x1 = min(pil.width, x1 + margin)
        y1 = min(pil.height, y1 + margin)
        pil = pil.crop((x0, y0, x1, y1))
    rgba = np.array(pil)
    pil.save(OUT_DIR / "wordmark.png")
    print(f"  wrote wordmark.png  cropped to {pil.size}")

    print("step 2: composite on ink + parchment for backdrop-specific variants")
    Image.fromarray(composite_on(rgba, INK)).save(OUT_DIR / "wordmark-vellum.png")
    print(f"  wrote wordmark-vellum.png (light wordmark on ink backdrop)")
    Image.fromarray(composite_on(rgba, PARCHMENT)).save(OUT_DIR / "wordmark-ink.png")
    print(f"  wrote wordmark-ink.png (wordmark on parchment backdrop)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
