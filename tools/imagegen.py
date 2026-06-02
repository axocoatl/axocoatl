#!/usr/bin/env python3
"""Generate images via OpenRouter.

OpenRouter exposes image generation through ``/api/v1/chat/completions``
(not the OpenAI ``/images/generations`` endpoint). Image-capable models
return images in ``choices[0].message.images[].image_url.url`` as either
``data:`` URIs or remote URLs.

We accept text prompts, optional reference images for image-to-image
edits, and save every returned image under ``branding/generated/``.

Usage:
    python tools/imagegen.py "your prompt here"
    python tools/imagegen.py "..." --model openai/gpt-5-image
    python tools/imagegen.py "edit this and add wings" --ref branding/glyph.svg
    python tools/imagegen.py "..." --n 3
"""
from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
DOTENV_CANDIDATES = [REPO_ROOT / ".env", REPO_ROOT / "tools" / ".env"]
OUT_DIR = REPO_ROOT / "branding" / "generated"

# OpenRouter image-output models (as listed by /api/v1/models?modality=image).
# Picking gemini-2.5-flash-image as the default — strong quality, fast,
# noticeably cheaper than the gpt-5-image family for prototyping.
DEFAULT_MODEL = "google/gemini-2.5-flash-image"


def load_env(path: Path) -> dict[str, str]:
    env: dict[str, str] = {}
    if not path.exists():
        return env
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def get_api_key() -> str:
    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        for cand in DOTENV_CANDIDATES:
            env = load_env(cand)
            if "OPENROUTER_API_KEY" in env:
                key = env["OPENROUTER_API_KEY"]
                break
    if not key:
        sys.exit(
            "OPENROUTER_API_KEY not found.\n"
            "  Looked in: "
            + ", ".join(str(c) for c in DOTENV_CANDIDATES)
            + "\n  Or export it in your shell.\n"
        )
    return key


def image_to_data_uri(path: Path) -> str:
    """Inline a local image file as a data: URI so we can pass it as a
    reference for image-to-image edits. SVGs are sent as image/svg+xml."""
    suffix = path.suffix.lower()
    if suffix == ".svg":
        mime = "image/svg+xml"
    else:
        mime = mimetypes.guess_type(str(path))[0] or "application/octet-stream"
    data = base64.b64encode(path.read_bytes()).decode()
    return f"data:{mime};base64,{data}"


def build_messages(prompt: str, refs: list[Path]) -> list[dict]:
    """Construct the chat messages. When no refs, a plain text message is
    sufficient. With refs, we use the multimodal content-array form."""
    if not refs:
        return [{"role": "user", "content": prompt}]
    content: list[dict] = [{"type": "text", "text": prompt}]
    for r in refs:
        content.append({
            "type": "image_url",
            "image_url": {"url": image_to_data_uri(r)},
        })
    return [{"role": "user", "content": content}]


def call_openrouter(messages: list[dict], model: str, n: int, key: str) -> dict:
    body = json.dumps(
        {
            "model": model,
            "messages": messages,
            "modalities": ["image", "text"],
            "n": n,
        }
    ).encode()
    req = urllib.request.Request(
        "https://openrouter.ai/api/v1/chat/completions",
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "HTTP-Referer": "https://github.com/your-org/axocoatl",
            "X-Title": "Axocoatl brand kit",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=240) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:500]
        sys.exit(f"OpenRouter HTTP {e.code}: {detail}")
    except urllib.error.URLError as e:
        sys.exit(f"OpenRouter network error: {e}")


def extract_images(data: dict) -> list[bytes]:
    """Pull bytes out of every image returned by the model. Handles both
    data:image/...;base64,... URIs and remote URLs."""
    out: list[bytes] = []
    for choice in data.get("choices") or []:
        msg = choice.get("message") or {}
        for img in msg.get("images") or []:
            url = (img.get("image_url") or {}).get("url") or img.get("url")
            if not url:
                continue
            if url.startswith("data:"):
                # data:image/png;base64,XXXX
                _, _, b64 = url.partition(",")
                out.append(base64.b64decode(b64))
            else:
                with urllib.request.urlopen(url, timeout=60) as r:
                    out.append(r.read())
    return out


def save(blobs: list[bytes], model: str) -> list[Path]:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    stamp = int(time.time())
    slug = model.split("/")[-1].replace(":", "-").replace(".", "-")
    paths: list[Path] = []
    for i, blob in enumerate(blobs):
        ext = ".png" if blob[:8] == b"\x89PNG\r\n\x1a\n" else ".jpg"
        p = OUT_DIR / f"{stamp}_{slug}_{i}{ext}"
        p.write_bytes(blob)
        paths.append(p)
    return paths


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate images via OpenRouter.")
    parser.add_argument("prompt", help="The text prompt.")
    parser.add_argument("--model", default=DEFAULT_MODEL,
                        help=f"OpenRouter model id (default: {DEFAULT_MODEL})")
    parser.add_argument("--ref", action="append", default=[], type=Path,
                        help="Reference image path(s) for image-to-image. Repeatable.")
    parser.add_argument("--n", type=int, default=1,
                        help="How many to generate (default: 1)")
    args = parser.parse_args()

    for r in args.ref:
        if not r.exists():
            sys.exit(f"reference not found: {r}")

    key = get_api_key()
    print(f"→ {args.model}  ·  n={args.n}  ·  refs={len(args.ref)}")
    print(f"  prompt: {args.prompt[:90]}{'…' if len(args.prompt) > 90 else ''}")
    messages = build_messages(args.prompt, args.ref)
    data = call_openrouter(messages, args.model, args.n, key)
    blobs = extract_images(data)
    if not blobs:
        # Useful when the model is verbose-text-only with no images.
        text = (data.get("choices") or [{}])[0].get("message", {}).get("content", "")
        sys.exit(f"No images in response. Text content was:\n{text[:400]}")
    for p in save(blobs, args.model):
        print(p)


if __name__ == "__main__":
    main()
