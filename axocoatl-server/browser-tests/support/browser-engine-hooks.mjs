// Module resolution hooks for browser-engine.mjs: every import of
// 'playwright' gets playwright-engine.mjs, except the one inside it, which
// gets Playwright itself.
const ENGINE_MODULE = new URL('./playwright-engine.mjs', import.meta.url).href;

export async function resolve(specifier, context, nextResolve) {
  if (specifier === 'playwright' && context.parentURL !== ENGINE_MODULE) {
    return { url: ENGINE_MODULE, shortCircuit: true };
  }
  return nextResolve(specifier, context);
}
