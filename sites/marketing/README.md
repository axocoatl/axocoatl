# Axocoatl marketing site

Vanilla HTML + Web Components + CSS. No framework. Production output is
the same as the source — every file in this directory ships as-is.

## Local preview

```bash
# Sync the canonical brand assets from ../../branding/ into assets/
./scripts/sync-assets.sh

# Any static server works. Python's is one line.
python3 -m http.server 8000

# Then open http://localhost:8000/
```

## Pages

| File              | Purpose                                            |
|-------------------|----------------------------------------------------|
| `index.html`      | Hero + 3 pillars + install snippet + lattice demo |
| `why.html`        | Positioning long-form, anonymous comparison table |
| `concepts.html`   | Mental model — lattice, agents, skills, sessions  |
| `showcase.html`   | Five concrete real-business workflows             |
| `install.html`    | curl / cargo / from-source instructions           |
| `_brand/`         | Hidden style guide. Not linked from nav. Designer reference. |

## Web Components

| Element                 | Purpose                                        |
|-------------------------|------------------------------------------------|
| `<ax-site-nav>`         | Sticky top nav with brand + links + CTA       |
| `<ax-finder-window>`    | macOS Finder–style card with title bar + slots|
| `<ax-pillar>`           | One column of the homepage 3-pillar grid      |
| `<ax-cli-snippet>`      | Copy-on-click terminal snippet                |
| `<ax-comparison-row>`   | One row of the us-vs-them table               |
| `<ax-theme-toggle>`     | Light/dark/system three-state toggle          |
| `<ax-lattice>`          | The actual product's lattice canvas, embedded |

`<ax-lattice>` is `@axocoatl/lattice@1` from npm, loaded via the
jsDelivr ESM CDN. Same package the dashboard ships.

## Brand tokens

`styles/tokens.css` is the single source of truth for CSS custom
properties on this site. The values come from `branding/colors.json` at
the repo root — keep them in sync. `sync-assets.sh` mirrors the rest of
the brand assets (mark.png, wordmark.png, favicon.png, colors.json)
into `assets/` at build time.

## Deploy

Cloudflare Pages targeting `axocoatl.ai`. See
`.github/workflows/marketing-deploy.yml` at the repo root.
