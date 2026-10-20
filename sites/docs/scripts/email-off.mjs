// Cloudflare's Email Address Obfuscation rewrites any text that looks like an email
// address on a served page, such as `axocoatl/axocoatl/actions/verify-receipt@v1.4.0`
// in a workflow or `e2e@0.18.0` in a recipe, into a "[email protected]" link. The
// copied workflow then still works, but the page shows the reader nothing to check.
// Cloudflare leaves alone whatever sits between <!--email_off--> and
// <!--/email_off-->, so every built page wraps its body in them; check-built.mjs
// refuses a page without them. The docs have no email address to protect.
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const EMAIL_OFF = '<!--email_off-->';
export const EMAIL_ON = '<!--/email_off-->';

const BODY_OPEN = /<body\b[^>]*>/i;

// Where the page's body starts and ends, or null when it has none.
function bodyRange(html) {
  const open = BODY_OPEN.exec(html);
  const close = html.toLowerCase().lastIndexOf('</body>');
  if (!open || close < open.index + open[0].length) return null;
  return { start: open.index + open[0].length, end: close };
}

// Whether the page's whole body is wrapped.
export function isEmailOff(html) {
  const range = bodyRange(html);
  if (!range) return false;
  const body = html.slice(range.start, range.end);
  return body.startsWith(EMAIL_OFF) && body.endsWith(EMAIL_ON);
}

// The page with its body wrapped, or unchanged when it has no body or is already
// wrapped.
export function wrapEmailOff(html) {
  const range = bodyRange(html);
  if (!range || isEmailOff(html)) return html;
  const { start, end } = range;
  return `${html.slice(0, start)}${EMAIL_OFF}${html.slice(start, end)}${EMAIL_ON}${html.slice(end)}`;
}

function htmlFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = join(directory, entry.name);
    if (entry.isDirectory()) return htmlFiles(target);
    return entry.name.endsWith('.html') ? [target] : [];
  });
}

// An Astro integration that wraps every built page.
export function emailOff() {
  return {
    name: 'axocoatl-email-off',
    hooks: {
      'astro:build:done': ({ dir, logger }) => {
        const files = htmlFiles(fileURLToPath(dir));
        for (const file of files) {
          const html = readFileSync(file, 'utf8');
          const wrapped = wrapEmailOff(html);
          if (wrapped !== html) writeFileSync(file, wrapped);
        }
        logger.info(`wrapped ${files.length} pages in email_off`);
      },
    },
  };
}
