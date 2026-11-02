// Every cover the dashboard draws comes from Octo's own address: the scripts never put a cover
// address saved from Deezer or Apple into the page, and a Fetched songs row (the part of admin.js
// from `function fetchedRow` to `async function loadFetched`) draws the `cover` Octo names.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const admin = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const listening = readFileSync(new URL('../listening.js', import.meta.url), 'utf8');

function slice(source, from, to) {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  assert.ok(start >= 0 && end > start, `${from} is in admin.js`);
  return source.slice(start, end);
}

test('no dashboard script draws a cover from another host', () => {
  for (const [name, source] of [['admin.js', admin], ['listening.js', listening]]) {
    assert.doesNotMatch(source, /coverArtUrl/, `${name} reads a saved outside cover address`);
    assert.doesNotMatch(source, /\.poster\b/, `${name} draws Apple's still itself`);
  }
});

function fetchedRow() {
  const block = slice(admin, 'const findArt = ', 'function lossyFindButton')
    + slice(admin, 'function fetchedRow(d)', 'async function loadFetched');
  const context = {
    escapeHtml: text => String(text ?? '').replace(/[&<>"']/g, c => `&#${c.charCodeAt(0)};`),
    fmtSize: () => '',
    relTime: () => '',
    fetchedFindButton: () => '',
    renderTagReport: () => '',
    fetchedLog: () => '',
  };
  vm.createContext(context);
  vm.runInContext(block, context);
  return context.fetchedRow;
}

test('a Fetched songs row draws the cover Octo serves', () => {
  const row = fetchedRow()({
    title: 'Glass',
    artist: 'Zephyr Echo',
    cover: '/api/admin/downloads/cover?at=2026-10-30T01%3A30%3A00.9766532Z',
    coverArtUrl: 'https://cdn-images.dzcdn.net/images/cover/x/1000x1000.jpg',
  });
  assert.match(row, /<img class="dl-art" src="\/api\/admin\/downloads\/cover\?at=2026-10-30T01%3A30%3A00\.9766532Z"/);
  assert.doesNotMatch(row, /dzcdn/);
});

test('a row Octo names no cover for keeps the placeholder tile', () => {
  const row = fetchedRow()({ title: 'Old song', coverArtUrl: 'https://cdn-images.dzcdn.net/x.jpg' });
  assert.match(row, /<div class="dl-art dl-art-ph"><\/div>/);
  assert.doesNotMatch(row, /<img/);
});
