// Run the browser regressions on another Playwright engine without changing
// the tests, which all import `chromium` from 'playwright'. run-tests.mjs
// loads this module (with --import) when AXO_BROWSER_ENGINE names another
// engine; it makes 'playwright' resolve to playwright-engine.mjs, whose
// `chromium` is that engine. AXO_BROWSER_ENGINE=webkit runs the suite on
// WebKit, the Safari engine (docs/RELEASE_GATE.md).
import { register } from 'node:module';

register('./browser-engine-hooks.mjs', import.meta.url);
