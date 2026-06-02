# Launch checklist

Pre-launch items that aren't urgent right now but must not get
forgotten before we flip the DNS at `axocoatl.ai` /
`docs.axocoatl.ai`. Read `BRAND.md` for voice, `launch/README.md`
for the actual day-of sequence.

Owner column: who flips the bit. `eng` = code change; `you` = a
human-only step (account, DNS, secret).

---

## Marketing site

- [x] **Clean URLs** — `/why`, `/concepts`, `/showcase`, etc.; no
      `.html` in any route. Files moved to `<name>/index.html`;
      all internal links rewritten. (2026-06-02)
- [x] **robots.txt + sitemap.xml** at `sites/marketing/`. (2026-06-02)
- [ ] **Swap vendored lattice → npm package on jsDelivr.** Today the
      marketing hero loads `<ax-scripted-lattice>` from
      `/vendor/lattice/index.js`, which `scripts/sync-assets.sh`
      copies out of `packages/lattice/src/` at deploy time. Once
      `@axocoatl/lattice` is published on npm under the
      `axocoatl` org and the CDN we trust is reachable from prod,
      flip `ax-scripted-lattice.js` to load from the public CDN
      and drop the local `/vendor/lattice/` copy. — eng
- [ ] **Generate OG images** for `/`, `/why`, `/concepts`,
      `/showcase`, `/install`, `/pricing`, `/changelog`,
      `/integrations/openrouter`. The `<meta property="og:image">`
      tags already point at `/assets/og-*.png` — those files don't
      exist yet. Slack/Twitter previews fall back to the favicon
      until they do. — eng
- [ ] **Screenshot the dashboard for `docs/img/dashboard.png`** so
      the README can embed a real product shot. — you
- [ ] **Real GitHub stars / downloads / testimonials.** Per
      BRAND.md §11, replace the factual trust row with real
      adoption signals once we cross 500+ stars. — eng

## Hosting + DNS

- [x] **Cloudflare account created** (2026-06-02). DNS + Pages
      projects below are next.
- [ ] **Register / point `axocoatl.ai` and `docs.axocoatl.ai`** at
      Cloudflare Pages. — you
- [ ] **Create the two Cloudflare Pages projects** named
      `axocoatl-marketing` and `axocoatl-docs` (referenced by
      `.github/workflows/marketing-deploy.yml` and
      `docs-deploy.yml`). — you
- [ ] **Add repo secrets** `CLOUDFLARE_API_TOKEN` and
      `CLOUDFLARE_ACCOUNT_ID` so the deploy workflows can push. — you

## GitHub

- [ ] **Create the public repo `github.com/axocoatl/axocoatl`** if
      it doesn't already exist. — you
- [ ] **Push the codebase + tag `v0.1.0`** to trigger the release
      matrix in `.github/workflows/release.yml`. — you
- [ ] **Enable GitHub Discussions** for the support channel
      referenced from `pricing.html` + `SECURITY.md`. — you
- [ ] **Add the `CRATES_IO_TOKEN` repo secret** so the release
      workflow can publish 21 crates to crates.io. — you

## npm

- [ ] **Publish `@axocoatl/lattice` to npm** under the
      `axocoatl` org so the marketing-site swap-back item above
      becomes possible. — you

## OpenRouter

- [ ] **Submit Axocoatl to OpenRouter's app directory** at
      <https://openrouter.ai/apps>. Once usage shows up the public
      listing will be linkable from the `/integrations/openrouter`
      page. — you

## Pre-launch quality gate

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Smoke every visible dashboard tab in a browser
- [ ] Open every marketing page and confirm: scripted lattice
      animates, install snippet copies, theme toggle persists,
      `/_brand` renders, footer links all 200, no console errors.
- [ ] Open every docs page; search works (Pagefind); the
      `<ax-lattice>` demo on `/concepts/lattice/` animates.
