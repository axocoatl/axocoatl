# Axocoatl documentation site

The public documentation for Axocoatl. The site is organized around complete
tasks: start the runtime, use the workbench, configure it, operate it, understand
its internals, and look up exact interfaces.

## Work locally

Run these commands from `sites/docs`:

```sh
npm ci
npm run check:content
npm run build
npm run check:links
npm run preview
```

`prebuild.sh` copies the canonical brand assets from `branding/` into the ignored
`public/` output before a build. Do not edit those generated copies.
The built-link check verifies internal `href`/`src` targets and fails when the
canonical favicon was not copied into `dist/`.

## Source discipline

- Product behavior comes from the current repository and `docs/PRODUCT.md`.
- Runtime behavior comes from `docs/ARCHITECTURE.md` and current source.
- Voice and visual identity come from `BRAND.md`.
- CLI and HTTP reference pages are checked against the command and router source by
  `npm run check:content`.

Do not publish, deploy, or change marketing copy from this package.
