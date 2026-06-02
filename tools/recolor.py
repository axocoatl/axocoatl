#!/usr/bin/env python3
"""Recolor a monochrome line-drawing into any target hex color.

The source PNG is usually a black-ink-on-white-background drawing
(transparent background isn't reliable from image-generation models),
so naïvely painting every non-transparent pixel with the target color
produces a solid block. Instead we treat the source as a luminance map:

    dark pixel  → high alpha, full target color
    light pixel → low alpha (transparent)
    grey pixel  → partial alpha so anti-aliased edges stay smooth

This works whether the source has a transparent background, a solid
white one, or any near-white shade.

Usage:
    python tools/recolor.py SRC OUT --color "#3E7C5C"
"""
from __future__ import annotations

import argparse
from pathlib import Path
from PIL import Image


def hex_to_rgb(hex_str: str) -> tuple[int, int, int]:
    s = hex_str.lstrip("#")
    if len(s) != 6:
        raise ValueError(f"hex must be 6 chars, got {hex_str!r}")
    return (int(s[0:2], 16), int(s[2:4], 16), int(s[4:6], 16))


def recolor(src: Path, out: Path, color: tuple[int, int, int]) -> None:
    """Map source luminance to alpha, then paint with `color`.

    For each pixel:
      L = perceived luminance of (R,G,B) on a 0..1 scale (Rec.709).
      ink = 1 - L  → strong where the source is dark, weak where it's light.
      alpha_out = source_alpha * ink * 255
      rgb_out = color

    Result: same line drawing, recolored, with smooth alpha edges on a
    transparent background — regardless of whether the source had a
    white or transparent backdrop.
    """
    img = Image.open(src).convert("RGBA")
    r_t, g_t, b_t = color
    pixels = img.load()
    w, h = img.size
    for y in range(h):
        for x in range(w):
            r, g, b, a = pixels[x, y]
            # Rec. 709 luminance. Pure black = 0, pure white = 255.
            lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
            ink = 1.0 - lum
            # Pixels that were already transparent stay transparent.
            new_a = int(round(a * ink))
            if new_a <= 0:
                pixels[x, y] = (0, 0, 0, 0)
            else:
                pixels[x, y] = (r_t, g_t, b_t, new_a)
    out.parent.mkdir(parents=True, exist_ok=True)
    img.save(out)
    print(f"  {out.name}  ({color[0]:02X}{color[1]:02X}{color[2]:02X})  {out.stat().st_size:,} bytes")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("src", type=Path)
    ap.add_argument("out", type=Path)
    ap.add_argument("--color", required=True, help="Hex color e.g. #3E7C5C")
    args = ap.parse_args()
    recolor(args.src, args.out, hex_to_rgb(args.color))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
