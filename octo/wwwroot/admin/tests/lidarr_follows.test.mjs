// Runs the Status page's Lidarr card block of admin.js (between `function renderLidarrFollows` and
// the settings' first line) against scripted fakes of the page and the admin API: each follow Octo
// cannot settle has its line and Put Lidarr back, and the button puts that follow back.
//
//   node --test octo/wwwroot/admin/tests/lidarr_follows.test.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');

function slice(from, to) {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  assert.ok(start > 0 && end > start, `${from} is in admin.js`);
  return source.slice(start, end);
}

const followsBlock = slice('function renderLidarrFollows(follows)', 'let currentSettings = null;');

const escapeHtml = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

function element(extra = {}) {
  const classes = new Set();
  return {
    hidden: false,
    innerHTML: '',
    textContent: '',
    disabled: false,
    dataset: {},
    parentElement: null,
    classList: {
      add: name => classes.add(name),
      remove: name => classes.delete(name),
      contains: name => classes.has(name),
    },
    addEventListener() {},
    querySelector() { return null; },
    ...extra,
  };
}

const CANNOT_SEE = 'Octo is still following a Lidarr download it cannot see';

// The block on a page holding Lidarr's card, the put-back answered by `reply`.
function page(reply = { status: 200, body: { status: 'ok' } }) {
  const holder = element({ hidden: true });
  const calls = { fetches: [], refreshed: 0 };
  const context = {
    document: { getElementById: id => (id === 'lidarr-follows' ? holder : null) },
    escapeHtml,
    icon: name => `<svg data-icon="${name}"></svg>`,
    parityFetch: async (path, options = {}, statusHolder) => {
      assert.ok(statusHolder, 'a holder for the sign-in to answer in');
      calls.fetches.push({ path, method: options.method || 'GET', body: options.body ? JSON.parse(options.body) : null });
      return { ok: reply.status === 200, status: reply.status, json: async () => reply.body };
    },
    parityJson: body => ({ method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }),
    parityError: async response => (await response.json()).error,
    refreshStatus: async () => { calls.refreshed += 1; },
  };
  vm.createContext(context);
  vm.runInContext(followsBlock, context);
  // A Put Lidarr back button as drawn, beside its status line.
  const button = follow => {
    const status = element();
    const row = element({ querySelector: selector => (selector === '.status-card-follow-status' ? status : null) });
    return { button: element({ dataset: { putBack: follow }, parentElement: row }), status };
  };
  return { context, holder, calls, button };
}

test('each follow Octo cannot settle has its line and Put Lidarr back; none hides the list', () => {
  const { context, holder } = page();
  context.renderLidarrFollows([
    { follow: 'rg-mezzanine', artist: 'Massive Attack', album: 'Mezzanine', text: CANNOT_SEE },
  ]);
  assert.equal(holder.hidden, false);
  assert.ok(holder.innerHTML.includes(`${CANNOT_SEE}: Massive Attack - Mezzanine`));
  assert.ok(holder.innerHTML.includes('data-put-back="rg-mezzanine"'));
  assert.ok(holder.innerHTML.includes('>Put Lidarr back<'));

  context.renderLidarrFollows(undefined);
  assert.equal(holder.hidden, true, 'an older server, or nothing to settle');
  context.renderLidarrFollows([]);
  assert.equal(holder.hidden, true);
});

test('Put Lidarr back puts that follow back and reads the status again', async () => {
  const { context, calls, button } = page();
  const { button: put } = button('rg-mezzanine');
  await context.putLidarrBack(put);
  assert.deepEqual(calls.fetches, [
    { path: '/api/admin/lidarr/put-back', method: 'POST', body: { follow: 'rg-mezzanine' } },
  ]);
  assert.equal(calls.refreshed, 1, 'the card is read again, without the follow');
});

test('a refused put back says why under the button, which can be pressed again', async () => {
  const { context, calls, button } = page({ status: 409, body: { error: 'Octo can see this download again, or its follow is over' } });
  const { button: put, status } = button('rg-mezzanine');
  await context.putLidarrBack(put);
  assert.equal(status.textContent, 'Octo can see this download again, or its follow is over');
  assert.ok(status.classList.contains('is-error'));
  assert.equal(put.disabled, false);
  assert.equal(calls.refreshed, 0);
});
