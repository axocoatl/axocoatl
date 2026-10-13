// Render the social cards from branding/launch-card.html with Playwright's
// Chromium. Uses the Playwright that the browser tests install:
//
//   (cd axocoatl-server/browser-tests && npm ci && npx playwright install chromium)
//   node branding/render-cards.mjs
//
// Writes the 1200x630 Open Graph card (sites/marketing/assets/og-home.png and
// branding/launch-card.png) and the 1280x640 GitHub social preview
// (branding/github-social-preview.png). The card loads Space Grotesk and
// JetBrains Mono from Google Fonts, so rendering needs the network.
import { copyFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const root = resolve(import.meta.dirname, '..');
const require = createRequire(resolve(root, 'axocoatl-server/browser-tests/package.json'));
const { chromium } = require('playwright');

const card = pathToFileURL(resolve(root, 'branding/launch-card.html')).href;
const outputs = [
  { width: 1200, height: 630, path: resolve(root, 'sites/marketing/assets/og-home.png') },
  { width: 1280, height: 640, path: resolve(root, 'branding/github-social-preview.png') },
];

const browser = await chromium.launch();
try {
  for (const { width, height, path } of outputs) {
    const page = await browser.newPage({ viewport: { width, height }, deviceScaleFactor: 1 });
    await page.goto(`${card}?w=${width}&h=${height}`, { waitUntil: 'networkidle' });
    await page.evaluate(() => document.fonts.ready);
    const missing = await page.evaluate(() =>
      ['600 76px "Space Grotesk"', '500 19px "JetBrains Mono"'].filter((font) => !document.fonts.check(font)));
    if (missing.length) throw new Error(`fonts did not load: ${missing.join(', ')}`);
    await page.screenshot({ path, clip: { x: 0, y: 0, width, height } });
    await page.close();
    console.log(`wrote ${path} (${width}x${height})`);
  }
} finally {
  await browser.close();
}
copyFileSync(outputs[0].path, resolve(root, 'branding/launch-card.png'));
console.log('copied the 1200x630 card to branding/launch-card.png');
