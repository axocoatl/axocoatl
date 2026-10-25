// static/ui/preview-embedding.js decides, without a browser, which browsers
// keep a Preview embedded (Safari's engine on an Apple platform) and which
// addresses the workbench then refuses to open in a tab of their own. The
// "Preview browser contract" journey (workbench.test.mjs) and
// preview-cookies.test.mjs drive the same code in WebKit and Chromium.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  claimsChromiumWithoutClientHints, guardTopLevelPreviews, isLocalhostSubdomain,
  isSafariEngineOnApple, previewsStayEmbeddedBecause, SAFARI_KEEPS_PREVIEWS_EMBEDDED,
  UNCONFIRMED_CHROMIUM_KEEPS_PREVIEWS_EMBEDDED,
} from '../../static/ui/preview-embedding.js';

const APPLE = 'Apple Computer, Inc.';
const GOOGLE = 'Google Inc.';
const MAC_SAFARI = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Safari/605.1.15';
const MAC_CHROME = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36';
const IPHONE_SAFARI = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1';

// What each browser reports as navigator.vendor, .platform and .userAgent.
// The Playwright rows were read from Playwright 1.64.1 on a Mac and on Linux
// (aarch64; x86_64 differs only in the platform's machine). Playwright's
// WebKit sends Safari's Mac user agent on Linux too, which is all the daemon
// sees (src/preview_document.rs), so the page takes it for Safari as well.
const PLAYWRIGHT_WEBKIT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.2 Safari/605.1.15';
const BROWSERS = [
  ['Safari on macOS', { vendor: APPLE, platform: 'MacIntel', userAgent: MAC_SAFARI }, true],
  ['Playwright WebKit 27.2 on macOS', { vendor: APPLE, platform: 'MacIntel', userAgent: PLAYWRIGHT_WEBKIT }, true],
  ['Playwright WebKit 27.2 on Linux', { vendor: APPLE, platform: 'Linux aarch64', userAgent: PLAYWRIGHT_WEBKIT }, true],
  ['Safari on iPhone', { vendor: APPLE, platform: 'iPhone', userAgent: IPHONE_SAFARI }, true],
  ['Safari on iPad, desktop site', { vendor: APPLE, platform: 'MacIntel', userAgent: MAC_SAFARI }, true],
  ['Chrome on iPhone (WebKit)', { vendor: APPLE, platform: 'iPhone', userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/141.0.7390.41 Mobile/15E148 Safari/604.1' }, true],
  ['Firefox on iPhone (WebKit)', { vendor: APPLE, platform: 'iPhone', userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/143.0 Mobile/15E148 Safari/605.1.15' }, true],
  ['Safari told to send Chrome\'s user agent', { vendor: APPLE, platform: 'MacIntel', userAgent: MAC_CHROME }, true],
  ['no vendor, Safari\'s user agent', { userAgent: MAC_SAFARI }, true],
  ['Chrome on macOS', { vendor: GOOGLE, platform: 'MacIntel', userAgent: MAC_CHROME }, false],
  ['Playwright Chromium (headless) on macOS', { vendor: GOOGLE, platform: 'MacIntel', userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/156.0.8078.4 Safari/537.36' }, false],
  ['Playwright Chromium (headless) on Linux', { vendor: GOOGLE, platform: 'Linux x86_64', userAgent: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/156.0.8078.4 Safari/537.36' }, false],
  ['Edge on macOS', { vendor: GOOGLE, platform: 'MacIntel', userAgent: `${MAC_CHROME} Edg/141.0.3537.57` }, false],
  ['Opera on macOS', { vendor: GOOGLE, platform: 'MacIntel', userAgent: `${MAC_CHROME} OPR/122.0.0.0` }, false],
  ['Chrome emulating an iPhone', { vendor: GOOGLE, platform: 'iPhone', userAgent: IPHONE_SAFARI }, false],
  ['Firefox on macOS', { vendor: '', platform: 'MacIntel', userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:143.0) Gecko/20100101 Firefox/143.0' }, false],
  ['WebKit on Linux with its own user agent', { vendor: APPLE, platform: 'Linux x86_64', userAgent: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.2 Safari/605.1.15' }, false],
  ['Chrome on Windows', { vendor: GOOGLE, platform: 'Win32', userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36' }, false],
  ['no navigator', undefined, false],
  ['an empty navigator', {}, false],
];

for (const [name, nav, expected] of BROWSERS) {
  test(`${name}: ${expected ? 'Safari\'s engine on an Apple platform' : 'not Safari\'s engine on an Apple platform'}`, () => {
    assert.equal(isSafariEngineOnApple(nav), expected);
  });
}

// navigator.userAgentData, which every Chromium-based browser has in a
// secure context (Playwright 1.64.1's Chromium 156 with its own user agent
// and with one Playwright set; Safari and Firefox have none).
const CHROMIUM_UA_DATA = { brands: [{ brand: 'Chromium', version: '156' }], mobile: false, platform: 'macOS' };
const FIREFOX_MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:143.0) Gecko/20100101 Firefox/143.0';

test('a browser that claims to be Chromium-based without its userAgentData keeps Previews embedded, in a secure context', () => {
  const chrome = { vendor: GOOGLE, platform: 'MacIntel', userAgent: MAC_CHROME, userAgentData: CHROMIUM_UA_DATA };
  const edge = { ...chrome, userAgent: `${MAC_CHROME} Edg/141.0.3537.57` };
  // Firefox told to send Chrome's user agent (navigator.vendor is empty).
  const firefoxAsChrome = { vendor: '', platform: 'MacIntel', userAgent: MAC_CHROME };
  const firefox = { vendor: '', platform: 'MacIntel', userAgent: FIREFOX_MAC };
  const safariAsChrome = { vendor: APPLE, platform: 'MacIntel', userAgent: MAC_CHROME };
  for (const nav of [chrome, edge, firefox]) {
    assert.equal(claimsChromiumWithoutClientHints(nav, true), false, nav.userAgent);
    assert.equal(previewsStayEmbeddedBecause(nav, true), null, nav.userAgent);
  }
  assert.equal(claimsChromiumWithoutClientHints(firefoxAsChrome, true), true);
  assert.equal(previewsStayEmbeddedBecause(firefoxAsChrome, true), UNCONFIRMED_CHROMIUM_KEEPS_PREVIEWS_EMBEDDED);
  assert.match(UNCONFIRMED_CHROMIUM_KEEPS_PREVIEWS_EMBEDDED, /^This browser says it is Chromium-based/);
  // Outside a secure context Chromium has no userAgentData either, so the
  // page cannot tell; the daemon still decides from its requests.
  assert.equal(claimsChromiumWithoutClientHints(firefoxAsChrome, false), false);
  assert.equal(previewsStayEmbeddedBecause({ ...chrome, userAgentData: undefined }, false), null);
  // Safari's own rule comes first, whatever it claims.
  assert.equal(previewsStayEmbeddedBecause(safariAsChrome, true), SAFARI_KEEPS_PREVIEWS_EMBEDDED);
  assert.equal(previewsStayEmbeddedBecause(undefined, true), null);
});

test('only a page on a subdomain of localhost counts as a Preview\'s address', () => {
  // The workbench's own address.
  const base = 'http://127.0.0.1:8300/?session=ses-1';
  for (const url of [
    'http://ses-123e4567-e89b-12d3-a456-426614174000-p3000.localhost:8300/app',
    'HTTP://SES-1-P3000.LOCALHOST:8300/',
    'http://ses-1-p3000.localhost.:8300/',
    'https://anything.localhost/',
    '//ses-1-p5173.localhost:8300/',
  ]) assert.equal(isLocalhostSubdomain(url, base), true, url);
  for (const url of [
    'http://localhost:8300/',
    'http://localhost:3000/app',
    'http://127.0.0.1:3000/',
    'http://127.0.0.1:8300/',
    'http://[::1]:3000/',
    'https://docs.axocoatl.ai/operate/security/',
    'http://localhost.example.com/',
    'http://ses-1-p3000.localhost.example.com/',
    '/api/sessions',
    'javascript:alert(1)',
    'not a url',
    '',
  ]) assert.equal(isLocalhostSubdomain(url, base), false, url);
  assert.equal(isLocalhostSubdomain('/relative'), false, 'a relative URL without a base');
});

/** A window with only what the guard uses, recording what it is asked. */
function fakeWindow(navigator) {
  const listeners = [];
  const opened = [];
  return {
    navigator,
    location: { href: 'http://127.0.0.1:8300/' },
    listeners,
    opened,
    addEventListener(type, listener, capture) { listeners.push({ type, listener, capture }); },
    open(...args) { opened.push(args); return 'a window'; },
  };
}

function linkEvent(href, button = 0) {
  const event = {
    button,
    defaultPrevented: false,
    composedPath: () => [
      { localName: 'span' },
      { localName: 'a', href, hasAttribute: (name) => name === 'href' },
      { localName: 'div' },
    ],
    preventDefault() { this.defaultPrevented = true; },
  };
  return event;
}

test('in Chromium the guard installs nothing', () => {
  const win = fakeWindow({ vendor: GOOGLE, platform: 'MacIntel', userAgent: MAC_CHROME, userAgentData: CHROMIUM_UA_DATA });
  win.isSecureContext = true;
  const open = win.open;
  assert.equal(guardTopLevelPreviews(win), false);
  assert.equal(win.open, open);
  assert.deepEqual(win.listeners, []);
});

test('in a browser that claims to be Chromium-based without being one, the guard refuses a Preview\'s address', () => {
  const win = fakeWindow({ vendor: '', platform: 'MacIntel', userAgent: MAC_CHROME });
  win.isSecureContext = true;
  assert.equal(guardTopLevelPreviews(win), true);
  assert.equal(win.open('http://ses-1-p3000.localhost:8300/'), null);
  assert.deepEqual(win.opened, []);
  assert.equal(win.open('https://docs.axocoatl.ai/'), 'a window');
});

test('in Safari the guard refuses links and window.open to a Preview\'s address, and nothing else', () => {
  const win = fakeWindow({ vendor: APPLE, platform: 'MacIntel', userAgent: MAC_SAFARI });
  assert.equal(guardTopLevelPreviews(win), true);
  assert.equal(guardTopLevelPreviews(win), true, 'installing twice is installing once');
  assert.deepEqual(win.listeners.map(({ type, capture }) => [type, capture]),
    [['click', true], ['auxclick', true], ['contextmenu', true], ['dragstart', true]]);

  const preview = 'http://ses-1-p3000.localhost:8300/app';
  assert.equal(win.open(preview, '_blank', 'noopener'), null);
  assert.equal(win.open(new URL(preview)), null, 'a URL object is read as its address');
  assert.deepEqual(win.opened, []);
  assert.equal(win.open('https://docs.axocoatl.ai', '_blank', 'noopener'), 'a window');
  assert.equal(win.open('/?session=ses-1', '_blank', 'noopener'), 'a window');
  assert.equal(win.open(), 'a window', 'an empty window opens as before');
  assert.deepEqual(win.opened, [
    ['https://docs.axocoatl.ai', '_blank', 'noopener'],
    ['/?session=ses-1', '_blank', 'noopener'],
    [undefined],
  ]);

  for (const { type, listener } of win.listeners) {
    const button = type === 'auxclick' ? 1 : 0;
    const toPreview = linkEvent(preview, button);
    listener(toPreview);
    assert.equal(toPreview.defaultPrevented, true, `${type} on a link to a Preview`);
    const toDocs = linkEvent('https://docs.axocoatl.ai/', button);
    listener(toDocs);
    assert.equal(toDocs.defaultPrevented, false, `${type} on a link to the docs`);
  }
  // A right click is also an auxclick; its contextmenu is what is refused.
  const rightClick = linkEvent(preview, 2);
  win.listeners.find(({ type }) => type === 'auxclick').listener(rightClick);
  assert.equal(rightClick.defaultPrevented, false);
});
