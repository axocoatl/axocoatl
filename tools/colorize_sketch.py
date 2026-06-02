#!/usr/bin/env python3
"""Colorize the Axocoatl sketch with PIXEL-EXACT line preservation AND
mark.png-style polished palette + finish.

Pipeline:
    SDXL base 1.0
  + ControlNet Canny SDXL   →  locks the sketch's lines at the pixel level
  + IP-Adapter Plus SDXL    →  pulls palette + scale rendering + bronze
                                metal finish + lighting from mark.png

ControlNet handles "every line of the sketch must survive"; IP-Adapter
handles "every pixel must look like it came from the same character
universe as mark.png." Text prompt adds nudges; mark.png is the
authoritative style reference.

Usage:
    python tools/colorize_sketch.py
    python tools/colorize_sketch.py --n 4 --ip-scale 0.8
"""
from __future__ import annotations

import argparse
import time
from pathlib import Path

import cv2
import numpy as np
import torch
from PIL import Image


REPO = Path(__file__).resolve().parent.parent
SKETCH = REPO / "branding" / "generated" / "1779674320_gemini-2-5-flash-image_0.png"
STYLE_REF = REPO / "branding" / "mark.png"
OUT_DIR = REPO / "branding" / "generated"

BASE_MODEL = "RunDiffusion/Juggernaut-XL-v9"
CONTROLNET_MODEL = "diffusers/controlnet-canny-sdxl-1.0"
VAE_MODEL = "madebyollin/sdxl-vae-fp16-fix"
IP_ADAPTER_REPO = "h94/IP-Adapter"
IP_ADAPTER_SUBFOLDER = "sdxl_models"
IP_ADAPTER_WEIGHTS = "ip-adapter_sdxl.safetensors"
# IP-Adapter for SDXL needs a CLIP ViT-bigG image encoder.  The repo
# bundles it under sdxl_models/image_encoder (not models/image_encoder
# which is the SD 1.5 ViT-H variant).
IP_ADAPTER_ENCODER_REPO = "h94/IP-Adapter"
IP_ADAPTER_ENCODER_SUBFOLDER = "sdxl_models/image_encoder"

PROMPT = (
    "premium brand mark, Aztec water serpent dragon ouroboros, "
    "rich jade green scales individually rendered, polished bronze gold "
    "metal head and ornament, bright cyan blue eye gem glow, studio "
    "illustration finish, isolated on white background"
)

NEGATIVE_PROMPT = (
    "stone carving, bas relief, sculpture, washed out, blurry, low quality, "
    "deformed, simple, flat, cartoon, anime, photograph, watermark, text, "
    "signature, frame, border, checkerboard, ornamental background"
)


def control_image(sketch_path: Path, size: int = 1024) -> Image.Image:
    """Tight-bbox crop + center-pad + invert the sketch for ControlNet."""
    img = np.array(Image.open(sketch_path).convert("L"))
    _, bw = cv2.threshold(img, 160, 255, cv2.THRESH_BINARY)
    dark = bw < 128
    ys, xs = np.where(dark)
    if len(xs):
        x0, x1 = xs.min(), xs.max()
        y0, y1 = ys.min(), ys.max()
        bw = bw[y0:y1 + 1, x0:x1 + 1]
    target = int(size * 0.85)
    h, w = bw.shape
    scale = target / max(h, w)
    new_h, new_w = int(h * scale), int(w * scale)
    bw = cv2.resize(bw, (new_w, new_h), interpolation=cv2.INTER_LANCZOS4)
    canvas = np.full((size, size), 255, dtype=np.uint8)
    oy, ox = (size - new_h) // 2, (size - new_w) // 2
    canvas[oy:oy + new_h, ox:ox + new_w] = bw
    _, canvas = cv2.threshold(canvas, 160, 255, cv2.THRESH_BINARY)
    inverted = 255 - canvas
    inverted = cv2.dilate(inverted, np.ones((2, 2), np.uint8), iterations=1)
    return Image.fromarray(cv2.cvtColor(inverted, cv2.COLOR_GRAY2RGB))


def run(n: int, steps: int, cn_strength: float, ip_scale: float, seed_start: int) -> list[Path]:
    print("loading ControlNet + VAE + SDXL…")
    from diffusers import (
        ControlNetModel,
        StableDiffusionXLControlNetPipeline,
        AutoencoderKL,
    )

    controlnet = ControlNetModel.from_pretrained(CONTROLNET_MODEL, torch_dtype=torch.float16)
    vae = AutoencoderKL.from_pretrained(VAE_MODEL, torch_dtype=torch.float16)
    pipe = StableDiffusionXLControlNetPipeline.from_pretrained(
        BASE_MODEL, controlnet=controlnet, vae=vae,
        torch_dtype=torch.float16, variant="fp16",
    )

    print("loading CLIP image encoder for IP-Adapter…")
    from transformers import CLIPVisionModelWithProjection
    image_encoder = CLIPVisionModelWithProjection.from_pretrained(
        IP_ADAPTER_ENCODER_REPO,
        subfolder=IP_ADAPTER_ENCODER_SUBFOLDER,
        torch_dtype=torch.float16,
    )
    pipe.image_encoder = image_encoder

    print("attaching IP-Adapter (style ref = mark.png)…")
    pipe.load_ip_adapter(
        IP_ADAPTER_REPO,
        subfolder=IP_ADAPTER_SUBFOLDER,
        weight_name=IP_ADAPTER_WEIGHTS,
    )
    pipe.set_ip_adapter_scale(ip_scale)

    pipe.enable_model_cpu_offload()
    pipe.vae.enable_tiling()

    control = control_image(SKETCH, size=1024)
    style_ref = Image.open(STYLE_REF).convert("RGB").resize((1024, 1024), Image.LANCZOS)

    written: list[Path] = []
    ts = int(time.time())
    for i in range(n):
        seed = seed_start + i
        gen = torch.Generator(device="cuda").manual_seed(seed)
        print(f"  [{i+1}/{n}] seed={seed} steps={steps} cn={cn_strength} ip={ip_scale}")
        out = pipe(
            prompt=PROMPT,
            negative_prompt=NEGATIVE_PROMPT,
            image=control,
            ip_adapter_image=style_ref,
            num_inference_steps=steps,
            controlnet_conditioning_scale=cn_strength,
            generator=gen,
            width=1024,
            height=1024,
        ).images[0]
        path = OUT_DIR / f"{ts}_cn-ip_seed{seed}_cn{int(cn_strength*100)}_ip{int(ip_scale*100)}_0.png"
        out.save(path)
        print(f"    → {path.name}")
        written.append(path)
    return written


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=4)
    ap.add_argument("--steps", type=int, default=35)
    ap.add_argument("--cn-strength", type=float, default=0.85,
                    help="ControlNet conditioning scale (1.0 = lines rigid)")
    ap.add_argument("--ip-scale", type=float, default=0.7,
                    help="IP-Adapter style scale (0.5-0.9 sweet spot)")
    ap.add_argument("--seed", type=int, default=300)
    args = ap.parse_args()
    paths = run(args.n, args.steps, args.cn_strength, args.ip_scale, args.seed)
    print(f"\nwrote {len(paths)} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
