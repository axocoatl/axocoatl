// Tests for scripts/email-off.mjs. Run: node --test ./scripts/email-off.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { EMAIL_OFF, EMAIL_ON, isEmailOff, wrapEmailOff } from './email-off.mjs';

const page = '<!DOCTYPE html><html><head><title>x</title></head><body class="docs"><pre>uses: a/b/verify-receipt@v1.4.0</pre></body></html>';

test('the whole body is wrapped, once', () => {
  const wrapped = wrapEmailOff(page);
  assert.equal(wrapped, `<!DOCTYPE html><html><head><title>x</title></head><body class="docs">${EMAIL_OFF}<pre>uses: a/b/verify-receipt@v1.4.0</pre>${EMAIL_ON}</body></html>`);
  assert.equal(wrapEmailOff(wrapped), wrapped);
  assert.equal(isEmailOff(wrapped), true);
  assert.equal(isEmailOff(page), false);
});

test('a page without a body is left alone and is not counted as wrapped', () => {
  const redirect = '<!doctype html><title>Redirecting</title><meta http-equiv="refresh" content="0;url=/start/install/">';
  assert.equal(wrapEmailOff(redirect), redirect);
  assert.equal(isEmailOff(redirect), false);
  assert.equal(isEmailOff('<html><body>no end'), false);
});

test('a body that only starts with the marker is not wrapped', () => {
  const half = `<html><body>${EMAIL_OFF}<p>a@b.c</p></body></html>`;
  assert.equal(isEmailOff(half), false);
  assert.equal(isEmailOff(wrapEmailOff(half)), true);
});
