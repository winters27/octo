// Runs the Library health lookup of fetched songs (the part of admin.js between
// `let healthFetchedToken` and the Find songs heading) against a scripted fake of the admin API.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const start = source.indexOf('let healthFetchedToken = 0;');
const end = source.indexOf('// ── Find songs');
assert.ok(start > 0 && end > start, 'the lookup block is in admin.js');
const block = source.slice(start, end);

// Each lookup's answer is held until the test lets it go, so answers can arrive in any order.
function load() {
  const out = { hidden: true, innerHTML: '' };
  const input = { value: '' };
  let submit = null;
  const form = { addEventListener: (kind, handler) => { if (kind === 'submit') submit = handler; } };
  const byId = { 'health-fetched-results': out, 'health-fetched-q': input, 'health-fetched-form': form };
  const asked = [];
  const context = {
    document: { getElementById: id => byId[id] ?? null },
    api: url => new Promise(resolve => asked.push({ url, answer: resolve })),
    stateBlock: (kind, text) => `[${kind}] ${text}`,
    fetchedRow: d => `<row ${d.title}>`,
    encodeURIComponent,
  };
  vm.createContext(context);
  vm.runInContext(block, context);
  return {
    out,
    asked,
    lookUp(q) {
      input.value = q;
      return submit({ preventDefault() {} });
    },
  };
}

const found = (...titles) => ({ ok: true, status: 200, json: async () => ({ downloads: titles.map(title => ({ title })) }) });

test('matches are listed with the Fetched songs row', async () => {
  const page = load();
  const done = page.lookUp('  daft punk ');
  assert.equal(page.out.hidden, false);
  assert.equal(page.out.innerHTML, '[loading] Looking through the fetched songs…');
  assert.equal(page.asked[0].url, '/api/admin/downloads?q=daft%20punk');
  page.asked[0].answer(found('Harder, Better, Faster, Stronger', 'One More Time'));
  await done;
  assert.equal(page.out.innerHTML, '<row Harder, Better, Faster, Stronger><row One More Time>');
});

test('blank text asks for words and calls nothing', async () => {
  const page = load();
  await page.lookUp('   ');
  assert.equal(page.out.innerHTML, '[empty] Type a title, an artist or both.');
  assert.equal(page.asked.length, 0);
});

test('no match and a failed call each say so', async () => {
  const page = load();
  const none = page.lookUp('nothing like it');
  page.asked[0].answer(found());
  await none;
  assert.ok(page.out.innerHTML.startsWith('[empty] No fetched song matches.'), page.out.innerHTML);

  const failed = page.lookUp('boom');
  page.asked[1].answer({ ok: false, status: 500, json: async () => ({}) });
  await failed;
  assert.equal(page.out.innerHTML, "[error] Couldn't search the fetched songs: HTTP 500");
});

test('a slow earlier lookup never replaces a later one', async () => {
  const page = load();
  const first = page.lookUp('halo');
  const second = page.lookUp('daft');
  page.asked[1].answer(found('Harder, Better, Faster, Stronger'));
  await second;
  page.asked[0].answer(found('Halo'));
  await first;
  assert.equal(page.out.innerHTML, '<row Harder, Better, Faster, Stronger>');

  const late = page.lookUp('halo');
  await page.lookUp('   ');
  page.asked[2].answer({ ok: false, status: 500, json: async () => ({}) });
  await late;
  assert.equal(page.out.innerHTML, '[empty] Type a title, an artist or both.', 'a late failure is dropped too');
});
