// Runs the dashboard's Radio status block (`loadRadioStatus` in admin.js) against a scripted
// answer from `/api/admin/lastfm/radio`.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');

function block(from, to) {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  assert.ok(start > 0 && end > start, `${from} is in admin.js`);
  return source.slice(start, end);
}

const status = block('async function loadRadioStatus()', "document.getElementById('radio-user')?.addEventListener");
const escape = block('function escapeHtml(s)', '\n}\n') + '\n}\n';

function element(extra = {}) {
  return { value: '', hidden: false, disabled: false, innerHTML: '', textContent: '', ...extra };
}

async function render(learning) {
  const output = element();
  const byId = {
    'radio-status': output,
    'radio-user': element(),
    'radio-listener-summary': element(),
    'radio-reset': element(),
  };
  const answer = {
    enabled: true,
    hasApiKey: true,
    selectedUser: 'alice',
    users: [{ username: 'alice' }],
    learning,
    stations: [{ name: 'Discovery Radio', kind: 'Discovery', trackCount: 25, preview: [] }],
  };
  const context = {
    document: { getElementById: id => byId[id] ?? null },
    api: async () => ({ ok: true, status: 200, json: async () => answer }),
    encodeURIComponent,
  };
  vm.createContext(context);
  vm.runInContext(escape + status, context);
  await vm.runInContext('loadRadioStatus()', context);
  return output.innerHTML;
}

const learned = { plays: 12, needed: 0, refreshing: false, lastRefreshError: null, carriedNote: null };
const note = "Couldn't update Discovery Radio, so it kept its songs";

test('a refresh that kept stations gets its own line and never reads as failed', async () => {
  const html = await render({ ...learned, lastRefreshError: note, carriedNote: note });
  assert.match(html, /<p class="radio-state-copy">12 completed plays learned\.<\/p>/);
  assert.match(
    html,
    /<p class="radio-state-copy">Couldn&#39;t update Discovery Radio, so it kept its songs\. Octo tries again every 15 minutes\.<\/p>/,
  );
  assert.doesNotMatch(html, /failed/);
});

test('the kept line shows while radio is still learning', async () => {
  const html = await render({ ...learned, plays: 4, needed: 6, lastRefreshError: note, carriedNote: note });
  assert.match(html, /4 completed plays learned · 6 more before Your Radio\./);
  assert.match(html, /Octo tries again every 15 minutes\./);
});

test('a real failure still reads as failed, beside the kept line', async () => {
  const html = await render({ ...learned, lastRefreshError: 'Provider returned no usable replacement stations', carriedNote: note });
  assert.match(html, /Last refresh failed: Provider returned no usable replacement stations/);
  assert.match(html, /Octo tries again every 15 minutes\./);
});

test('nothing kept, no kept line', async () => {
  const html = await render(learned);
  assert.doesNotMatch(html, /tries again/);
});
