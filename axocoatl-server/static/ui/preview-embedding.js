/**
 * In Safari, a Preview stays embedded in the workbench.
 *
 * Safari's engine on macOS lets a page on a `*.localhost` address that runs
 * top-level set a cookie for `Domain=localhost` from script, and then sends it
 * to the workbench on `localhost` and to every Session's Preview
 * (operate/security.mdx, "Safari shares Domain=localhost cookies"). Embedded
 * in the workbench's frame, Safari gives a Preview no cookies at all. So where
 * the browser is Safari's engine on an Apple platform, the workbench never
 * runs a Preview top-level:
 * - `<ax-browser>` does not offer Open full preview, and its frame may not
 *   open windows (no `allow-popups`);
 * - `guardTopLevelPreviews` keeps every link and `window.open` in the
 *   workbench (chat, citations, the file editor's links) from opening a
 *   `*.localhost` address in a tab or window of its own;
 * - `<ax-terminal>` does the same for links a program prints.
 * Every other browser keeps its behaviour. This module only decides; it
 * changes nothing until a caller asks it to.
 */

/** Why, in the words the workbench shows. */
export const PREVIEW_STAYS_EMBEDDED = 'In Safari, a Preview in its own tab could set cookies '
  + 'for the workbench and for other Sessions, so Previews stay in this pane. For a full-page '
  + 'Preview, open the workbench in another browser.';

/** The event the shell turns into a notice when a link was not opened. */
export const PREVIEW_STAYS_EMBEDDED_EVENT = 'preview-stays-embedded';

/**
 * True when `nav` is Safari's engine, WebKit, on macOS, iOS or iPadOS: Safari,
 * Playwright's WebKit on a Mac, and every browser on iOS. False for
 * Chromium-based browsers (Chrome, Edge, Opera, Brave, Arc), for Firefox, and
 * for WebKit on Linux, which refuses a `Domain=localhost` cookie.
 *
 * `navigator.vendor` names the engine: "Apple Computer, Inc." in WebKit,
 * "Google Inc." in Chromium, empty in Firefox. Only when it is empty or
 * missing does the user agent decide. Either the platform or the user agent
 * naming an Apple device is enough.
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
 * True when `url`, resolved against `base`, is a page on a subdomain of
 * `localhost`, where every Preview runs (`<session>-p<port>.localhost`).
 * `localhost` itself, the workbench's host, is not one.
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

/** True in this browser: Safari's engine on an Apple platform. */
export const KEEPS_PREVIEWS_EMBEDDED = isSafariEngineOnApple(globalThis.navigator);

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
 * In Safari, keep the workbench's links and `window.open` from opening a
 * `*.localhost` address in a tab or window of its own. A click (with or
 * without a modifier), a middle click, the link's context menu and dragging
 * it out are all refused; the shell is told, once per refused click. Returns
 * whether it guarded anything (false in every other browser).
 *
 * @param {Window} win
 * @returns {boolean}
 */
export function guardTopLevelPreviews(win = globalThis) {
  if (!isSafariEngineOnApple(win.navigator)) return false;
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
