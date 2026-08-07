/**
 * Shared constructable stylesheets for `ax-*` components.
 *
 * Custom properties and `@font-face` both cross a shadow boundary — measured,
 * not assumed — so components get the brand palette and the icon font for free
 * simply by using `var(--accent)` and `font-family: codicon`.
 *
 * Selector rules do **not** cross. The codicon glyph classes
 * (`.codicon-folder::before { content: "\ea83" }`) are selector rules, so every
 * shadow root that draws an icon needs them. Rather than each component
 * shipping its own copy of a 200 KB sheet, the CSS text is fetched once and
 * parsed into one `CSSStyleSheet` object that every root adopts. Adopting is by
 * reference: N components share one parsed sheet, and there is no `<link>` per
 * root to re-parse or flash.
 */

/** One in-flight fetch per URL, shared by every caller. */
const cache = new Map();

/**
 * Fetch a stylesheet once and return it as an adoptable `CSSStyleSheet`.
 *
 * @param {string} url
 * @returns {Promise<CSSStyleSheet>}
 */
export function sharedSheet(url) {
  if (!cache.has(url)) {
    cache.set(
      url,
      fetch(url)
        .then((r) => {
          if (!r.ok) throw new Error(`${url} → ${r.status}`);
          return r.text();
        })
        .then((css) => {
          const sheet = new CSSStyleSheet();
          sheet.replaceSync(css);
          return sheet;
        }),
    );
  }
  return cache.get(url);
}

/** The vendored @vscode/codicons glyph classes. */
export const CODICONS = '/vendor/codicons/codicon.css';

/**
 * Reduced motion, inside every shadow root.
 *
 * The document's `prefers-reduced-motion` block cannot reach in here — selector
 * rules stop at the boundary — so a component's own keyframes would keep
 * running for someone who asked the OS for less movement. The rail's status
 * dots are the sharp case: they pulse *forever*, so ignoring the preference
 * means a permanently animating page.
 *
 * Adopted before each component's own rules, and deliberately `!important`, so
 * a component cannot re-enable motion by being more specific.
 */
const REDUCED_MOTION = `
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: .001ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: .001ms !important;
  }
}`;

const motionSheet = new CSSStyleSheet();
motionSheet.replaceSync(REDUCED_MOTION);

/**
 * Adopt shared sheets into a shadow root, then the component's own rules.
 *
 * Order matters: shared first so a component can override it. Awaiting means a
 * root can render before its icons resolve, which is correct — text appears
 * immediately and glyphs fill in, rather than the pane withholding itself.
 *
 * @param {ShadowRoot} root
 * @param {string} ownCss       this component's rules
 * @param {string[]} sharedUrls stylesheets to adopt before them
 */
export async function adopt(root, ownCss, sharedUrls = [CODICONS]) {
  const own = new CSSStyleSheet();
  own.replaceSync(ownCss);
  // The motion sheet is last so it wins over the component's own transitions,
  // and it is applied on the first pass too — a root that renders before its
  // icons land must not animate in the meantime.
  root.adoptedStyleSheets = [own, motionSheet];
  const shared = await Promise.all(sharedUrls.map(sharedSheet));
  root.adoptedStyleSheets = [...shared, own, motionSheet];
}
