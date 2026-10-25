/**
 * In Safari, a Preview stays embedded in the workbench.
 *
 * Safari's engine on macOS lets a page on a `*.localhost` address that runs
 * top-level set a cookie for `Domain=localhost` from script, and then sends it
 * to every other app on a `localhost` address, other Sessions' Previews
 * included (operate/security.mdx, "Safari shares Domain=localhost cookies").
 * The workbench, at `127.0.0.1`, never receives it. Embedded in the
 * workbench's frame, Safari gives a Preview no cookies at all. So where
 * the browser is Safari's engine on an Apple platform, or claims to be a
 * Chromium-based browser without being one (`claimsChromiumWithoutClientHints`),
 * the workbench never runs a Preview top-level:
 * - `<ax-browser>` does not offer Open full preview, and its frame may not
 *   open windows (no `allow-popups`);
 * - `guardTopLevelPreviews` keeps every link and `window.open` in the
 *   workbench (chat, citations, the file editor's links) from opening a
 *   `*.localhost` address in a tab or window of its own;
 * - `<ax-terminal>` does the same for links a program prints.
 * Every other browser keeps its behaviour. The daemon refuses a Preview page
 * that would run top-level in Safari anyway, however it was opened
 * (src/preview_document.rs, which decides by the user agent); this keeps the
 * workbench from offering one and says why. This module only decides; it
 * changes nothing until a caller asks it to.
 */

/** Why, in the words the workbench shows, in Safari. */
export const SAFARI_KEEPS_PREVIEWS_EMBEDDED = 'In Safari, a Preview in its own tab could set cookies '
  + 'for other Sessions and other apps on localhost, so Previews stay in this pane. For a full-page '
  + 'Preview, open the workbench in another browser.';

/** Why, where the browser claims to be Chromium-based without being one. */
export const UNCONFIRMED_CHROMIUM_KEEPS_PREVIEWS_EMBEDDED = 'This browser says it is '
  + 'Chromium-based but does not identify itself as Chromium-based browsers do, so it is taken '
  + 'for Safari, where a Preview in its own tab could set cookies for other Sessions and other '
  + 'apps on localhost: Previews stay in this pane. For a full-page Preview, open the workbench '
  + 'in another browser, or in this one with its own user agent.';

/** The event the shell turns into a notice when a link was not opened. */
export const PREVIEW_STAYS_EMBEDDED_EVENT = 'preview-stays-embedded';

/**
 * True when `nav` is Safari's engine, WebKit, on macOS, iOS or iPadOS: Safari,
 * Playwright's WebKit, and every browser on iOS. False for Chromium-based
 * browsers (Chrome, Edge, Opera, Brave, Arc), for Firefox, and for WebKit on
 * Linux with its own user agent (`X11; Linux`), which refuses a
 * `Domain=localhost` cookie.
 *
 * `navigator.vendor` names the engine: "Apple Computer, Inc." in WebKit,
 * "Google Inc." in Chromium, empty in Firefox. Only when it is empty or
 * missing does the user agent decide. Either the platform or the user agent
 * naming an Apple device is enough, so the page never offers what the daemon,
 * which sees only the user agent, refuses: Playwright's WebKit on Linux
 * reports a Linux `navigator.platform` but sends Safari's Mac user agent, so
 * both take it for Safari.
 *
 * @param {{vendor?: string, userAgent?: string, platform?: string} | undefined} nav
 * @returns {boolean}
 */
export function isSafariEngineOnApple(nav) {
  const text = (value) => (typeof value === 'string' ? value : '');
  const vendor = text(nav?.vendor);
  const userAgent = text(nav?.userAgent);
  const platform = text(nav?.platform);
  const webkit = vendor
    ? vendor.startsWith('Apple')
    : /AppleWebKit\//.test(userAgent) && !/(?:Chrome|Chromium|Edg|OPR|Firefox)\//.test(userAgent);
  const apple = /^(?:Mac|iPhone|iPad|iPod)/.test(platform)
    || /\b(?:Macintosh|iPhone|iPad|iPod)\b/.test(userAgent);
  return webkit && apple;
}

/**
 * True when the user agent of `nav` names a Chromium-based browser (Chrome,
 * Chromium, Edge, Opera) but `nav` has no `userAgentData`, in a secure
 * context (`secure`), where every Chromium-based browser has it: a Safari or
 * Firefox told to send that user agent. The daemon refuses such a browser's
 * Preview pages the same way, by the client hints its requests lack
 * (src/preview_document.rs). Outside a secure context nothing tells; the
 * daemon still decides.
 *
 * @param {{userAgent?: string, userAgentData?: object} | undefined} nav
 * @param {boolean} secure
 * @returns {boolean}
 */
export function claimsChromiumWithoutClientHints(nav, secure) {
  const userAgent = typeof nav?.userAgent === 'string' ? nav.userAgent : '';
  return Boolean(secure) && /(?:Chrome|Chromium|Edg|OPR)\//.test(userAgent) && !nav?.userAgentData;
}

/**
 * Whether this browser keeps Previews embedded, and why in its words, or
 * `null` when it does not.
 *
 * @param {object | undefined} nav
 * @param {boolean} secure
 * @returns {string | null}
 */
export function previewsStayEmbeddedBecause(nav, secure) {
  if (isSafariEngineOnApple(nav)) return SAFARI_KEEPS_PREVIEWS_EMBEDDED;
  if (claimsChromiumWithoutClientHints(nav, secure)) return UNCONFIRMED_CHROMIUM_KEEPS_PREVIEWS_EMBEDDED;
  return null;
}

/**
 * True when `url`, resolved against `base`, is a page on a subdomain of
 * `localhost`, where every Preview runs (`<session>-p<port>.localhost`).
 * `localhost` itself is not one.
 *
 * @param {string} url
 * @param {string} [base]
 * @returns {boolean}
 */
export function isLocalhostSubdomain(url, base) {
  try {
    const hostname = new URL(String(url), base).hostname.replace(/\.$/, '');
    return hostname.endsWith('.localhost');
  } catch {
    return false;
  }
}

const BECAUSE = previewsStayEmbeddedBecause(globalThis.navigator, globalThis.isSecureContext);

/** True in this browser: Safari's engine on an Apple platform, or a browser taken for it. */
export const KEEPS_PREVIEWS_EMBEDDED = BECAUSE !== null;

/** Why Previews stay embedded in this browser, in the words the workbench shows. */
export const PREVIEW_STAYS_EMBEDDED = BECAUSE ?? SAFARI_KEEPS_PREVIEWS_EMBEDDED;

/** True when this browser must not open `url` in a tab or window of its own. */
export function refusesTopLevel(url) {
  return KEEPS_PREVIEWS_EMBEDDED && isLocalhostSubdomain(url, globalThis.location?.href);
}

/** Tell the shell a link was not opened, so it can say why. */
export function announcePreviewStaysEmbedded() {
  globalThis.dispatchEvent?.(new CustomEvent(PREVIEW_STAYS_EMBEDDED_EVENT, {
    detail: { reason: PREVIEW_STAYS_EMBEDDED },
  }));
}

/**
 * In Safari, and a browser taken for it, keep the workbench's links and
 * `window.open` from opening a `*.localhost` address in a tab or window of
 * its own. A click (with or without a modifier), a middle click, the link's
 * context menu and dragging it out are all refused; the shell is told, once
 * per refused click. Returns whether it guarded anything (false in every
 * other browser).
 *
 * @param {Window} win
 * @returns {boolean}
 */
export function guardTopLevelPreviews(win = globalThis) {
  if (previewsStayEmbeddedBecause(win.navigator, win.isSecureContext) === null) return false;
  if (win.__axTopLevelPreviewGuard) return true;
  win.__axTopLevelPreviewGuard = true;
  const refusedLink = (event) => event.composedPath().find((node) =>
    (node?.localName === 'a' || node?.localName === 'area')
    && node.hasAttribute('href')
    && isLocalhostSubdomain(node.href, win.location.href));
  for (const type of ['click', 'auxclick', 'contextmenu', 'dragstart']) {
    win.addEventListener(type, (event) => {
      // A right click is an auxclick as well as a contextmenu; only a middle
      // click opens a link.
      if (type === 'auxclick' && event.button !== 1) return;
      if (!refusedLink(event)) return;
      event.preventDefault();
      if (type !== 'dragstart') announcePreviewStaysEmbedded();
    }, true);
  }
  const open = win.open;
  win.open = function openUnlessPreview(url, ...rest) {
    if (url != null && url !== '' && isLocalhostSubdomain(url, win.location.href)) {
      announcePreviewStaysEmbedded();
      return null;
    }
    return Reflect.apply(open, win, [url, ...rest]);
  };
  return true;
}
