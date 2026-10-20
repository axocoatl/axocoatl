// Playwright, with `chromium` replaced by the engine AXO_BROWSER_ENGINE
// names (chromium, the default, or webkit). The options only Chromium
// understands (executablePath, which on macOS points at Google Chrome; args;
// channel; chromiumSandbox) are dropped when another engine launches.
import * as playwright from 'playwright';

export * from 'playwright';

export const ENGINES = ['chromium', 'webkit'];

/** The engine `value` names; an unknown one is an error that lists the others. */
export function selectedEngine(value) {
  const engine = value || 'chromium';
  if (!ENGINES.includes(engine)) {
    throw new Error(`AXO_BROWSER_ENGINE=${engine} is not one of ${ENGINES.join(', ')}`);
  }
  return engine;
}

/** Launch options without the Chromium-only ones. */
export function engineLaunchOptions(options) {
  if (!options) return options;
  const { executablePath, args, channel, chromiumSandbox, ...rest } = options;
  return rest;
}

export const engine = selectedEngine(process.env.AXO_BROWSER_ENGINE);

function asEngine(browserType) {
  if (engine === 'chromium') return browserType;
  return new Proxy(browserType, {
    get(target, property) {
      if (property === 'launch' || property === 'launchServer') {
        return (options) => target[property](engineLaunchOptions(options));
      }
      if (property === 'launchPersistentContext') {
        return (userDataDir, options) => target[property](userDataDir, engineLaunchOptions(options));
      }
      const value = Reflect.get(target, property, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
}

export const chromium = asEngine(playwright[engine]);
