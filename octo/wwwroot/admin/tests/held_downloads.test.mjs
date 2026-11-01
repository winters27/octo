// Runs two blocks of admin.js against scripted fakes of the page and the admin API: the Status
// page's downloads card (the part between `function renderDownloadChecks` and the status refresh
// button's listener), and a held In progress card's Keep anyway and Stop waiting for Lidarr menus
// (between `const ACQ_MENUS` and the Fetched songs log).
//
//   node --test octo/wwwroot/admin/tests/held_downloads.test.mjs

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

const checksBlock = slice('function renderDownloadChecks(lines)', "document.getElementById('status-refresh')");
const menusBlock = slice('const ACQ_MENUS = {', '// Fetched songs: the log folds under the row');

function element(extra = {}) {
  return {
    hidden: false,
    innerHTML: '',
    checked: false,
    disabled: false,
    dataset: {},
    addEventListener() {},
    closest() { return null; },
    querySelector() { return null; },
    ...extra,
  };
}

const escapeHtml = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

// The downloads card's block, with the settings save answered by `reply`.
function checksPage(reply = { status: 200, body: { ok: true } }) {
  const wrap = element({ hidden: true });
  const list = element();
  const settingsBox = element();
  const calls = { posts: [], refreshed: 0, notes: [] };
  const context = {
    document: {
      getElementById: id => ({ 'download-checks': wrap, 'download-checks-list': list }[id] ?? null),
      querySelectorAll: selector => (selector === '[name="Soulseek.VerifyDownloads"]' ? [settingsBox] : []),
    },
    api: async (url, options = {}) => {
      calls.posts.push({ url, method: options.method, body: JSON.parse(options.body) });
      return { ok: reply.status === 200, status: reply.status, json: async () => reply.body };
    },
    escapeHtml,
    icon: name => `<svg data-icon="${name}"></svg>`,
    note: (anchor, message, kind) => calls.notes.push({ message, kind }),
    refreshStatus: async () => { calls.refreshed += 1; },
    currentSettings: { Soulseek: { VerifyDownloads: false, MinMatchScore: 85 } },
  };
  vm.createContext(context);
  vm.runInContext(checksBlock, context);
  return { context, wrap, list, settingsBox, calls };
}

const VERIFICATION_OFF = {
  id: 'verificationOff',
  text: 'Octo is not checking imported downloads by their sound. Turn it on',
  warning: true,
  turnOn: 'Soulseek.VerifyDownloads',
};

test('the downloads card shows each line, and the verification line carries its switch', () => {
  const page = checksPage();
  page.context.renderDownloadChecks([
    VERIFICATION_OFF,
    { id: 'acoustIdOff', text: 'AcoustID is off: Octo checks what played against the catalog\'s own clip of each song, and keeps a copy only when it is the very master you played', warning: false },
  ]);
  assert.equal(page.wrap.hidden, false);
  assert.ok(page.list.innerHTML.includes('Octo is not checking imported downloads by their sound. Turn it on'));
  assert.ok(page.list.innerHTML.includes('data-turn-on="Soulseek.VerifyDownloads"'));
  assert.ok(page.list.innerHTML.includes('AcoustID is off: Octo checks what played'));
  assert.equal(page.list.innerHTML.match(/data-turn-on=/g).length, 1, 'only the verification line has a switch');
  assert.ok(page.list.innerHTML.includes('is-warning'));

  page.context.renderDownloadChecks([]);
  assert.equal(page.wrap.hidden, true, 'nothing to say hides the card');
});

test('the switch saves verification on, as the Verify downloads card does, then reads the status again', async () => {
  const page = checksPage();
  const input = element({ checked: true, dataset: { turnOn: 'Soulseek.VerifyDownloads' } });
  await page.context.turnOnDownloadCheck(input);
  assert.deepEqual(page.calls.posts, [
    { url: '/api/admin/settings', method: 'POST', body: { Soulseek: { VerifyDownloads: true } } },
  ]);
  assert.equal(page.calls.refreshed, 1);
  assert.equal(page.settingsBox.checked, true, "the settings page's switch says the same");
  assert.equal(vm.runInContext('currentSettings.Soulseek.VerifyDownloads', page.context), true);
  assert.equal(vm.runInContext('currentSettings.Soulseek.MinMatchScore', page.context), 85, 'the rest kept');
});

test('a refused save puts the switch back and says why', async () => {
  const page = checksPage({ status: 409, body: { error: 'settings.json is not valid JSON.' } });
  const input = element({ checked: true, dataset: { turnOn: 'Soulseek.VerifyDownloads' } });
  await page.context.turnOnDownloadCheck(input);
  assert.equal(input.checked, false);
  assert.equal(input.disabled, false);
  assert.equal(page.calls.refreshed, 0);
  assert.deepEqual(page.calls.notes, [{ message: 'Not saved: settings.json is not valid JSON.', kind: 'error' }]);
});

// The menus' block. `answers` maps a path to its reply ({status, body}); every call is recorded.
function menusPage(answers) {
  const bodies = [];
  const calls = { fetches: [], reloads: 0 };
  const context = {
    document: {
      getElementById: () => element(),
      querySelectorAll: selector => (selector === '[data-acq-menu-body]' ? bodies : []),
    },
    parityFetch: async (path, options = {}, holder) => {
      assert.ok(holder, 'a holder for the sign-in to answer in');
      calls.fetches.push({ path, method: options.method || 'GET', body: options.body ? JSON.parse(options.body) : null });
      const reply = answers[path.split('?')[0]] ?? { status: 404, body: { error: 'not here' } };
      return { ok: reply.status === 200, status: reply.status, json: async () => reply.body };
    },
    parityJson: body => ({ method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }),
    parityError: async response => (await response.json()).error,
    loadAcquisitions: async () => { calls.reloads += 1; },
    escapeHtml,
    icon: name => `<svg data-icon="${name}"></svg>`,
    stateBlock: (kind, message) => `<div class="state state-${kind}">${escapeHtml(message)}</div>`,
  };
  vm.createContext(context);
  vm.runInContext(menusBlock, context);
  // The page as drawn: one open menu's details and its body.
  const open = (kind, key) => {
    const body = element({ dataset: { acqMenuBody: kind, acqKey: key } });
    bodies.push(body);
    const details = element({ open: true, dataset: { acqMenu: kind, acqKey: key }, querySelector: () => body });
    return { details, body };
  };
  const pick = (kind, key, user, body) =>
    context.acqMenuAct(element({ dataset: { acqAct: kind, acqKey: key, acqUser: user }, closest: () => body }));
  return { context, calls, open, pick };
}

const settle = () => new Promise(resolve => setImmediate(resolve));

test('a held row offers Keep anyway, Keep these anyway for two or more held tracks, and Stop waiting for Lidarr', () => {
  const { context } = menusPage({});
  const song = context.acqHeldMenus({ key: 'soulseek:help', keepAnyway: true });
  assert.ok(song.includes('>Keep anyway<'));
  const one = context.acqHeldMenus({ key: 'soulseek:album', keepAnyway: true, heldTracks: 1 });
  assert.ok(one.includes('>Keep anyway<'), "one held track's note already says keep it anyway");
  const album = context.acqHeldMenus({ key: 'soulseek:album', keepAnyway: true, heldTracks: 3 });
  assert.ok(album.includes('>Keep these anyway<'));
  const waits = context.acqHeldMenus({ key: 'soulseek:teardrop', stopWaiting: true });
  assert.ok(waits.includes('>Stop waiting for Lidarr<'));
  assert.ok(!waits.includes('Keep'));
  assert.equal(context.acqHeldMenus({ key: 'soulseek:angel' }), '', 'nothing held offers nothing');
});

test('the holder menu reads who may keep, and a name picked keeps for that person', async () => {
  const page = menusPage({
    '/api/admin/acquisitions/holders': { status: 200, body: { key: 'soulseek:help', keepers: ['alice', 'bob'], stopWaiters: [] } },
    '/api/admin/acquisitions/keep': { status: 200, body: { status: 'ok' } },
  });
  const { details, body } = page.open('keep', 'soulseek:help');
  page.context.acqMenuToggled(details);
  await settle();
  assert.deepEqual(page.calls.fetches, [
    { path: '/api/admin/acquisitions/holders?key=soulseek%3Ahelp', method: 'GET', body: null },
  ]);
  assert.ok(body.innerHTML.includes('Whose copy to keep'));
  assert.ok(body.innerHTML.includes('data-acq-user="alice"') && body.innerHTML.includes('data-acq-user="bob"'));

  // A redraw of the open menu opens it again; nothing new is read.
  page.context.acqMenuToggled(details);
  await settle();
  assert.equal(page.calls.fetches.length, 1);

  await page.pick('keep', 'soulseek:help', 'bob', body);
  assert.deepEqual(page.calls.fetches[1], {
    path: '/api/admin/acquisitions/keep',
    method: 'POST',
    body: { key: 'soulseek:help', user: 'bob' },
  });
  assert.equal(page.calls.reloads, 1, 'the list is drawn again');
  assert.equal(page.context.acqMenuMarkup('soulseek:help', 'keep', 'Keep anyway').includes(' open'), false, 'the menu closes once kept');
});

test('a refused keep shows its words in the menu, which stays open', async () => {
  const page = menusPage({
    '/api/admin/acquisitions/holders': { status: 200, body: { keepers: ['alice'], stopWaiters: [] } },
    '/api/admin/acquisitions/keep': { status: 409, body: { error: 'Nothing is waiting to be kept' } },
  });
  const { details, body } = page.open('keep', 'soulseek:help');
  page.context.acqMenuToggled(details);
  await settle();
  await page.pick('keep', 'soulseek:help', 'alice', body);
  const redrawn = page.context.acqMenuMarkup('soulseek:help', 'keep', 'Keep anyway');
  assert.ok(redrawn.includes(' open'));
  assert.ok(redrawn.includes('Nothing is waiting to be kept'));
});

test('Stop waiting for Lidarr picks from whose kept copy waits, and stops for that person', async () => {
  const page = menusPage({
    '/api/admin/acquisitions/holders': { status: 200, body: { keepers: [], stopWaiters: ['alice'] } },
    '/api/admin/acquisitions/stop-waiting': { status: 200, body: { status: 'ok' } },
  });
  const { details, body } = page.open('stop', 'soulseek:teardrop');
  page.context.acqMenuToggled(details);
  await settle();
  assert.ok(body.innerHTML.includes('Whose copy should stop waiting'));
  await page.pick('stop', 'soulseek:teardrop', 'alice', body);
  assert.deepEqual(page.calls.fetches[1], {
    path: '/api/admin/acquisitions/stop-waiting',
    method: 'POST',
    body: { key: 'soulseek:teardrop', user: 'alice' },
  });
});

test('a menu with nobody left says nothing waits', async () => {
  const page = menusPage({
    '/api/admin/acquisitions/holders': { status: 200, body: { keepers: [], stopWaiters: [] } },
  });
  const keep = page.open('keep', 'soulseek:help');
  page.context.acqMenuToggled(keep.details);
  const stop = page.open('stop', 'soulseek:help');
  page.context.acqMenuToggled(stop.details);
  await settle();
  assert.ok(keep.body.innerHTML.includes('Nothing is waiting to be kept'));
  assert.ok(stop.body.innerHTML.includes('Nothing is waiting for Lidarr'));
});
