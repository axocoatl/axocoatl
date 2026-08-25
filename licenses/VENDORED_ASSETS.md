# Embedded browser asset provenance

Axocoatl embeds every file under `axocoatl-server/static/vendor/` in the
native server binary. The table below records the exact upstream release,
source payload, and license for all 161 embedded third-party files. The
checked-in `licenses/vendor-assets.sha256` file is the byte-level inventory;
`scripts/check-third-party-licenses.sh` refuses an added, removed, or changed
asset until that inventory and this review are updated together.

| Embedded files | Files | Exact upstream payload | License text |
| --- | ---: | --- | --- |
| `monaco/vs/**` | 151 | Full AMD `min/vs/**` graph built from [`monaco-editor` tag `v0.56.0`](https://github.com/microsoft/monaco-editor/releases/tag/v0.56.0), with its vendored `monaco-editor-core@0.56.0-dev-20260625` DOMPurify source replaced byte-for-byte by `dompurify@3.4.13` before the pinned Vite build. Exact inputs, source hashes, build steps, and output hashes are in `vendor-web/monaco-build.json`; the audited npm graph is locked in `vendor-web/package-lock.json`. | `vendor/monaco-editor-MIT.txt`, `vendor/monaco-editor-ThirdPartyNotices.txt`, `vendor/dompurify-3.4.13-NOTICE.txt`, `vendor/dompurify-3.4.13-Apache-2.0.txt`, `vendor/dompurify-3.4.13-MPL-2.0.txt` |
| `codicons/codicon.css` | 1 | `@vscode/codicons@0.0.45`, `package/dist/codicon.css`, [npm tarball](https://registry.npmjs.org/@vscode/codicons/-/codicons-0.0.45.tgz) | `vendor/codicons-code-MIT.txt` |
| `codicons/codicon.ttf` | 1 | `@vscode/codicons@0.0.45` (font version 1.15), `package/dist/codicon.ttf`, [npm tarball](https://registry.npmjs.org/@vscode/codicons/-/codicons-0.0.45.tgz) | `vendor/codicons-font-CC-BY-4.0.txt` |
| `fonts/jetbrains-mono-var.woff2` | 1 | `@fontsource-variable/jetbrains-mono@5.3.0`, `package/files/jetbrains-mono-latin-wght-normal.woff2`, [npm tarball](https://registry.npmjs.org/@fontsource-variable/jetbrains-mono/-/jetbrains-mono-5.3.0.tgz) | `vendor/jetbrains-mono-OFL-1.1.txt` |
| `fonts/space-grotesk-var.woff2` | 1 | `@fontsource-variable/space-grotesk@5.3.0`, `package/files/space-grotesk-latin-wght-normal.woff2`, [npm tarball](https://registry.npmjs.org/@fontsource-variable/space-grotesk/-/space-grotesk-5.3.0.tgz) | `vendor/space-grotesk-OFL-1.1.txt` |
| `highlight.min.js` | 1 | `@highlightjs/cdn-assets@11.10.0`, `package/highlight.min.js`, [npm tarball](https://registry.npmjs.org/@highlightjs/cdn-assets/-/cdn-assets-11.10.0.tgz) | `vendor/highlight-js-BSD-3-Clause.txt` |
| `highlight.css` | 1 | `@highlightjs/cdn-assets@11.10.0`, `package/styles/github-dark.min.css`, [npm tarball](https://registry.npmjs.org/@highlightjs/cdn-assets/-/cdn-assets-11.10.0.tgz) | `vendor/highlight-js-BSD-3-Clause.txt` |
| `markdown-it.min.js` | 1 | `markdown-it@14.3.0`, `package/dist/markdown-it.min.js`, [npm tarball](https://registry.npmjs.org/markdown-it/-/markdown-it-14.3.0.tgz) | `vendor/markdown-it-MIT.txt` |
| `xterm.js` | 1 | `xterm@5.3.0`, [jsDelivr `lib/xterm.min.js`](https://cdn.jsdelivr.net/npm/xterm@5.3.0/lib/xterm.min.js) | `vendor/xterm-MIT.txt` |
| `xterm.css` | 1 | `xterm@5.3.0`, [jsDelivr `css/xterm.min.css`](https://cdn.jsdelivr.net/npm/xterm@5.3.0/css/xterm.min.css) | `vendor/xterm-MIT.txt` |
| `xterm-addon-fit.js` | 1 | `xterm-addon-fit@0.8.0`, [jsDelivr `lib/xterm-addon-fit.min.js`](https://cdn.jsdelivr.net/npm/xterm-addon-fit@0.8.0/lib/xterm-addon-fit.min.js) | `vendor/xterm-addon-fit-MIT.txt` |

The Monaco payload is the complete 151-file output graph from the documented
source build; no transitive hashed chunk or worker was selected away. It uses
DOMPurify 3.4.13 because the latest published Monaco 0.56.0 payload embeds
DOMPurify 3.4.8, which downstream consumers cannot override
([microsoft/monaco-editor#5454](https://github.com/microsoft/monaco-editor/issues/5454)),
while the reviewed
[GHSA-55q2-fjhq-7xh7](https://github.com/cure53/DOMPurify/security/advisories/GHSA-55q2-fjhq-7xh7)
affects DOMPurify through 3.4.12 and identifies 3.4.13 as the patched release.
Every other row was compared
byte-for-byte with the single upstream path named in the table. The source
archives are not required during a normal build: the SHA-256 inventory makes
the reviewed local bytes the deterministic release input, while the versioned
URLs and build metadata preserve provenance for a future refresh.

The two Codicons licenses are intentionally separate. The CSS/code is MIT.
The unmodified CC BY 4.0 font is titled **Codicons — The icon font for Visual
Studio Code**, is attributed to Microsoft Corporation and contributors, and
links its exact source above. The two font families retain their exact upstream
OFL copyright notices in their respective license files.
