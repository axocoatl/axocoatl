# Axocoatl brand kit

Single canonical mark.  Every brand asset descends from `mark.png`.

## Files

| File              | Type | Used at      | Used where                                           |
|-------------------|------|--------------|------------------------------------------------------|
| `mark.png`        | PNG  | any size     | THE brand mark — the Aztec water-serpent ouroboros.  |
| `favicon.png`     | PNG  | 16–128 px    | Browser tab icon, derived from `mark.png` at 128 px. |
| `wordmark.png`    | PNG  | any size     | `ax` + mark + `atl` in Space Grotesk SemiBold.       |
| `wordmark-ink.png`| PNG  | any size     | Wordmark composited on parchment background.         |
| `wordmark-vellum.png` | PNG | any size  | Wordmark composited on ink background.               |
| `colors.json`     | JSON | code         | Palette source of truth.                             |
| `SERPENT-AXIOM.md`| MD   | docs         | Character spec — drives every serpent generation.    |
| `launch-card.html`| HTML | source       | Source of the social cards below.                    |
| `launch-card.png` | PNG  | 1200×630     | Open Graph card; the site ships it as `sites/marketing/assets/og-home.png`. |
| `github-social-preview.png` | PNG | 1280×640 | GitHub repository social preview (uploaded by hand in the repository settings). |

## Palette

- **Jade Green** `#3E7C5C` — primary, serpent body
- **Bronze Gold** `#B5904A` — secondary, metalwork / crest / chasing
- **Circuit Blue** `#3FA9C8` — accent, eye glow
- **Ink** `#0E1218`, **Stone** `#1B2027`, **Parchment** `#F4ECDA`, **Vellum** `#FFFFFF` — neutrals

## How the mark was made

The current `mark.png` comes from a three-reference workflow in
`tools/imagegen.py`:

1. `branding/sketch-reference.png` — the canonical line-art sketch (composition lock).
2. The previous coloured iteration in `branding/generated/` (palette guide).
3. The accepted final from the same gallery (style guide).

Each gen was passed `--ref` for all three; the chosen iteration was
processed through `tools/knockout_bg.py` to strip the model-drawn
checker background to clean alpha, then promoted to `branding/mark.png`.

## How the wordmark is made

`tools/wordmark.py` composes the mark inline between the letters `ax`
and `atl` (Space Grotesk SemiBold).  Re-run after any `mark.png`
replacement to regenerate `wordmark.png` + the ink/vellum variants.

## How the social cards are made

`launch-card.html` lays out the card at any size given as `?w=…&h=…`.
`node branding/render-cards.mjs` renders it with the Playwright Chromium the
browser tests install (`cd axocoatl-server/browser-tests && npm ci && npx
playwright install chromium`) and writes `sites/marketing/assets/og-home.png`,
`launch-card.png` and `github-social-preview.png`. It loads Space Grotesk and
JetBrains Mono from Google Fonts, so it needs the network. Re-render after
changing the card's copy, and keep the `og:image:alt` text of the marketing
pages in step with it.

## Single-mark rule

There is exactly one mark.  No "no-cpu" variant, no mono variants, no
emblem variant — `mark.png` is the answer for every brand surface.
Favicon + wordmark are mechanically derived from it.
