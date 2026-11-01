// Runs the Mornye now card's and the Pass panel's rules (the part of admin.js from
// `function mornyeWhen` to `let mornyeNowLoad`) and the setup checklist's heart source rule.
//
//   node --test "octo/wwwroot/admin/tests/*.test.mjs"

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');

function slice(from, to) {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  assert.ok(start > 0 && end > start, `${from} is in admin.js`);
  return source.slice(start, end);
}

const escLine = slice('const esc = (s)', 'async function refreshLibraryStatus');
const esc = new Function(`${escLine}; return esc;`)();
const icon = name => `<svg data-icon="${name}"></svg>`;
const rules = new Function(
  'esc',
  'icon',
  `${slice('function mornyeWhen', 'let mornyeNowLoad')}
   return { mornyeWhen, mornyeCard, mornyeLinesHtml, passZoomDefault, passPoint, passView, passInputQueue, passViewer, passViewerShown, passLifecycle, passCalls };`,
)(esc, icon);
const { mornyeWhen, mornyeCard, mornyeLinesHtml, passZoomDefault, passPoint, passView, passInputQueue, passViewer, passViewerShown, passLifecycle, passCalls } = rules;

const heartSourceReachable = new Function(
  'lastStatus',
  'currentSettings',
  `${slice('function heartSourceReachable()', 'function renderSetupChecklist')} return heartSourceReachable();`,
);

// Local times, so the words read the same in any time zone the tests run in.
const now = new Date(2026, 10, 1, 14, 0, 0);
const local = (...parts) => new Date(...parts).toISOString();
// Times keep their parts together with no-break spaces; the expectations below are written with
// plain ones.
const plain = text => text.replace(/ /g, ' ');
const when = (iso, roundUp = false) => plain(mornyeWhen(iso, now, roundUp, 'en-US'));
const texts = card => card.lines.map(line => plain(line.text));

const ready = {
  on: true,
  reachable: true,
  name: 'Mornye',
  version: '1.0.5',
  webUrl: 'http://octo.lan:8010',
  problem: null,
  utilitiesMissing: false,
  reportsCheck: true,
  services: [{ id: 'example-ext', label: 'Example' }],
  extensions: [{ name: 'example-ext', state: 'ok', lastError: null }],
  checks: [],
  checkSongs: 0,
  helper: 'ok',
};
const paused = {
  ...ready,
  checks: [{
    label: 'Example',
    extensions: ['example-ext'],
    pausedUntilUtc: local(2026, 10, 1, 15, 40, 30),
    since: local(2026, 9, 31, 22, 5, 0),
    gatewayAsked: false,
  }],
  checkSongs: 2,
  paused: true,
  passable: true,
};

test('times read the time alone today, the date on another day, the year in another year', () => {
  assert.equal(when(local(2026, 10, 1, 15, 40, 0)), '3:40 PM');
  assert.equal(when(local(2026, 10, 2, 9, 5, 0)), 'Nov 2, 9:05 AM');
  assert.equal(when(local(2027, 0, 3, 9, 5, 0)), 'Jan 3, 2027, 9:05 AM');
  assert.equal(when(null), '');
});

test("a wait's end rounds up to the next minute, so the next try is never early", () => {
  assert.equal(when(local(2026, 10, 1, 15, 40, 30), true), '3:41 PM');
  assert.equal(when(local(2026, 10, 1, 15, 40, 0), true), '3:40 PM');
  assert.equal(when(local(2026, 10, 1, 15, 40, 30)), '3:40 PM', 'a start time is not rounded');
});

test('a time never wraps between its hour and its AM or PM', () => {
  assert.equal(mornyeWhen(local(2026, 10, 1, 15, 40, 0), now, false, 'en-US'), '3:40 PM');
});

test('the card is one quiet line while Mornye is off', () => {
  const card = mornyeCard({ on: false }, now);
  assert.equal(card.tone, 'off');
  assert.equal(card.lines.length, 0);
  assert.equal(card.canPass, false);
});

test('an unreachable Mornye says so with its problem', () => {
  const card = mornyeCard({ ...paused, reachable: false, problem: 'Mornye did not answer at http://spotiflac:8000' }, now);
  assert.equal(card.tone, 'bad');
  assert.equal(card.title, 'Octo cannot reach Mornye.');
  assert.deepEqual(card.lines.map(line => [line.state, line.text]), [['fail', 'Mornye did not answer at http://spotiflac:8000.']]);
  assert.equal(card.canPass, false, 'nothing to pass while Mornye does not answer');
});

test('missing utilities stop the card at that problem', () => {
  const card = mornyeCard({ ...paused, version: '1.1.0', utilitiesMissing: true }, now);
  assert.equal(card.tone, 'bad');
  assert.equal(card.title, 'Mornye 1.1.0 answers but cannot download.');
  assert.ok(card.lines.some(line => line.text === 'Mornye needs its utilities extension, which your registry does not list.'));
  assert.equal(card.canPass, false);
});

test('a ready Mornye names itself and its version', () => {
  const card = mornyeCard(ready, now);
  assert.equal(card.tone, 'ok');
  assert.equal(card.title, 'Mornye 1.0.5 is ready.');
  assert.equal(card.webUrl, 'http://octo.lan:8010');
  assert.equal(card.lines.length, 0);
});

test('a paused check says since when and the next try, in local time', () => {
  const card = mornyeCard(paused, now);
  assert.equal(card.tone, 'warn');
  assert.equal(card.title, 'Mornye 1.0.5 answers, but its check is paused.');
  assert.deepEqual(texts(card), [
    'Mornye has not passed its check for Example since Oct 31, 10:05 PM; next try 3:41 PM.',
  ]);
});

test('a pause the gateway asked for says so instead of a next try', () => {
  const card = mornyeCard({ ...paused, checks: [{ ...paused.checks[0], gatewayAsked: true, since: null }] }, now);
  assert.deepEqual(texts(card), [
    "Mornye's check for Example is paused; the gateway asked it to wait until 3:41 PM.",
  ]);
});

test('failing extensions, an unreported check and a missing token each get their line', () => {
  const card = mornyeCard({
    ...ready,
    reportsCheck: false,
    helper: 'no token',
    extensions: [
      { name: 'example-ext', state: 'failing', lastError: 'verification paused' },
      { name: 'other-ext', state: 'untried', lastError: null },
    ],
  }, now);
  assert.equal(card.title, 'Mornye 1.0.5 answers, but an extension failed its last try.');
  assert.deepEqual(card.lines.map(line => [line.state, line.text]), [
    ['warn', 'example-ext failed its last try: verification paused.'],
    ['info', 'This Mornye does not report its check.'],
    ['info', 'Set a web token for Mornye (SPOTIFLAC_WEB_TOKEN) to pass its check from Octo.'],
  ]);
});

test("a helper that refuses the token says so, not that it is missing", () => {
  const card = mornyeCard({ ...paused, helper: 'refused', passable: false }, now);
  assert.ok(card.lines.some(line => line.text === "Octo's helper beside Mornye refused the web token. Octo and Mornye must share SPOTIFLAC_WEB_TOKEN."), JSON.stringify(card.lines));
  assert.ok(!card.lines.some(line => line.text.includes('spotiflac service')));
});

test("Pass shows exactly when the server says it can help, the status grid's rule too", () => {
  assert.equal(mornyeCard(paused, now).canPass, true);
  assert.equal(mornyeCard({ ...paused, passable: false }, now).canPass, false);
  assert.equal(mornyeCard({ ...paused, passable: undefined }, now).canPass, false, 'an older server offers no Pass');
  assert.ok(mornyeCard({ ...paused, checkSongs: 0, passable: false }, now).lines.some(line => line.text.startsWith('No song has asked Mornye')));
  assert.ok(mornyeCard({ ...paused, helper: 'absent' }, now).lines.some(line => line.text.includes('spotiflac service')));
});

test("Mornye's own words are escaped where the card draws them", () => {
  const card = mornyeCard({
    ...paused,
    checks: [{ ...paused.checks[0], label: '<img src=x onerror=alert(1)>' }],
    extensions: [{ name: 'a&b', state: 'failing', lastError: '<script>bad()</script>' }],
  }, now);
  const html = mornyeLinesHtml(card.lines);
  assert.ok(!html.includes('<img'), 'no tag from a label');
  assert.ok(!html.includes('<script>'), 'no tag from an error');
  assert.ok(html.includes('&lt;img src=x onerror=alert(1)&gt;'));
  assert.ok(html.includes('a&amp;b failed its last try: &lt;script&gt;'));
});

test('a narrow screen starts the zoom at 3x, a wide one at 1x', () => {
  assert.equal(passZoomDefault(375), 3);
  assert.equal(passZoomDefault(599), 3);
  assert.equal(passZoomDefault(600), 1);
  assert.equal(passZoomDefault(1280), 1);
});

// The browser's layout as the panel builds it: the box is the card's width at the frame's own
// shape, the picture is `zoom` times the box's width, and the box scrolls over it.
function drawn(box, zoom, scroll, frame) {
  const width = box.width * zoom;
  const height = width * (frame.height / frame.width);
  return { left: box.left - scroll.left, top: box.top - scroll.top, width, height };
}
const frame = { width: 1279, height: 756 };
// Where a spot of the frame shows on screen for that layout.
const onScreen = (rect, x, y) => [rect.left + (x / frame.width) * rect.width, rect.top + (y / frame.height) * rect.height];

test("a click on the check's box reaches the same spot in the frame at 1x on a wide screen", () => {
  const rect = drawn({ left: 312, top: 420, width: 880 }, 1, { left: 0, top: 0 }, frame);
  const [clientX, clientY] = onScreen(rect, 61, 73);
  assert.deepEqual({ ...passPoint(clientX, clientY, rect, frame.width, frame.height) }, { x: 61, y: 73 });
});

test('a tap at 3x on a 375 px screen, scrolled, reaches the same spot in the frame', () => {
  const box = { left: 36, top: 500, width: 303 };
  const rect = drawn(box, 3, { left: 120, top: 40 }, frame);
  const [clientX, clientY] = onScreen(rect, 300, 200);
  assert.deepEqual({ ...passPoint(clientX, clientY, rect, frame.width, frame.height) }, { x: 300, y: 200 });

  // The scrolling box's own rectangle (the zoom and the scroll left out) lands somewhere else.
  const boxRect = { left: box.left, top: box.top, width: box.width, height: box.width * (frame.height / frame.width) };
  assert.notDeepEqual({ ...passPoint(clientX, clientY, boxRect, frame.width, frame.height) }, { x: 300, y: 200 });
});

test('a pointer dragged past the picture stays inside the frame', () => {
  const rect = { left: 0, top: 0, width: 640, height: 378 };
  assert.deepEqual({ ...passPoint(-30, 900, rect, frame.width, frame.height) }, { x: 0, y: 755 });
  assert.deepEqual({ ...passPoint(700, -5, rect, frame.width, frame.height) }, { x: 1278, y: 0 });
});

function fakeLine({ fail = () => false } = {}) {
  let clock = 0;
  const sent = [];
  const failures = [];
  const queue = passInputQueue(async event => {
    sent.push({ ...event, at: clock });
    await Promise.resolve();
    if (fail(event, sent)) throw new Error('refused');
  }, {
    now: () => clock,
    sleep: async ms => { clock += ms; },
    failed: (event, error) => failures.push([event.type, error.message]),
  });
  return { queue, sent, failures };
}

test('pointer events go in order, waiting moves merge, presses and releases are never dropped', async () => {
  const { queue, sent } = fakeLine();
  queue.push({ type: 'mouseMoved', x: 1, y: 1 });
  queue.push({ type: 'mouseMoved', x: 2, y: 2 });
  queue.push({ type: 'mouseMoved', x: 3, y: 3 });
  queue.push({ type: 'mousePressed', x: 3, y: 3 });
  queue.push({ type: 'mouseMoved', x: 4, y: 4 });
  queue.push({ type: 'mouseMoved', x: 5, y: 5 });
  queue.push({ type: 'mouseReleased', x: 5, y: 5 });
  await queue.idle();

  assert.deepEqual(sent.map(event => `${event.type}@${event.x}`), [
    'mouseMoved@1',
    'mouseMoved@3',
    'mousePressed@3',
    'mouseMoved@5',
    'mouseReleased@5',
  ]);
  const moves = sent.filter(event => event.type === 'mouseMoved').map(event => event.at);
  moves.slice(1).forEach((at, i) => assert.ok(at - moves[i] >= 50, 'at most one move every 50 ms'));
});

test('each post waits for the one before it', async () => {
  let inFlight = 0;
  let most = 0;
  const queue = passInputQueue(async () => {
    inFlight += 1;
    most = Math.max(most, inFlight);
    await new Promise(resolve => setTimeout(resolve, 2));
    inFlight -= 1;
  }, { gap: 0 });
  ['mousePressed', 'mouseMoved', 'mouseReleased', 'mousePressed', 'mouseReleased']
    .forEach(type => queue.push({ type, x: 0, y: 0 }));
  await queue.idle();
  assert.equal(most, 1);
});

test('a failed release is sent once more; a failed press is not', async () => {
  const { queue, sent, failures } = fakeLine({
    fail: (event, all) => all.filter(seen => seen.type === event.type).length === 1,
  });
  queue.push({ type: 'mousePressed', x: 9, y: 9 });
  queue.push({ type: 'mouseReleased', x: 9, y: 9 });
  await queue.idle();
  assert.deepEqual(sent.map(event => event.type), ['mousePressed', 'mouseReleased', 'mouseReleased']);
  assert.deepEqual(failures, [['mousePressed', 'refused']]);
});

test('a release refused twice is reported once', async () => {
  const { queue, sent, failures } = fakeLine({ fail: event => event.type === 'mouseReleased' });
  queue.push({ type: 'mouseReleased', x: 1, y: 1 });
  await queue.idle();
  assert.equal(sent.length, 2);
  assert.deepEqual(failures, [['mouseReleased', 'refused']]);
});

test("the panel's words follow the run's state", () => {
  assert.equal(passView({ state: 'opening' }, now).text, "Opening Mornye's check…");
  assert.equal(passView({ state: 'opening' }, now).polls, true);
  assert.equal(passView({ state: 'live' }, now).live, true);
  assert.equal(passView({ state: 'passed' }, now).text,
    'Passed. Songs ask Mornye again from now on; ones it could not get during the pause need a heart again.');
  assert.equal(passView({ state: 'passed' }, now).polls, false);
  assert.equal(passView({ state: 'closed' }, now).retry, 'Try again');
  assert.equal(passView({ state: 'no-check', checkSongs: 1 }, now).retry, 'Try another song');
  assert.equal(passView({ state: 'no-check', checkSongs: 0 }, now).retry, null, 'no other song, no button');
  assert.equal(passView({ state: 'unavailable', why: 'Set a web token.' }, now).text, 'Set a web token.');
  assert.equal(passView({ state: 'unavailable', why: 'Set a web token.' }, now).retry, null);
  assert.deepEqual(passView({ state: 'live', gatewayAsked: [{ gateway: 'g', until: local(2026, 10, 1, 16, 0, 0) }] }, now).notes.map(plain),
    ['The gateway asked Mornye to wait until 4:00 PM.']);
});

// A sessionStorage stand-in. `copy` is what Duplicate tab does: the new tab starts with the old
// one's entries.
function memoryStore(entries = {}) {
  const map = new Map(Object.entries(entries));
  return {
    getItem: key => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => { map.set(key, String(value)); },
    removeItem: key => { map.delete(key); },
    copy: () => memoryStore(Object.fromEntries(map)),
  };
}
const ids = () => { let n = 0; return () => `id-${++n}`; };

test('a reload is the same tab on a new page; a duplicated tab is a tab of its own', () => {
  const store = memoryStore();
  const first = passViewer(store, ids());
  assert.equal(first.tab, 'id-2');
  assert.equal(first.page, 'id-1');

  // A reload: the page leaves (its mark cleared), the next load finds the tab.
  passViewerShown(store, first, false);
  const reloaded = passViewer(store, () => 'page-again');
  assert.deepEqual(reloaded, { tab: first.tab, page: 'page-again' });

  // Duplicate tab while this one is open: the copy finds the mark still set.
  const copy = store.copy();
  const duplicated = passViewer(copy, () => 'other');
  assert.notEqual(duplicated.tab, reloaded.tab);
  // The first tab keeps its own name.
  assert.equal(store.getItem('octo.passTab'), reloaded.tab);

  // Brought back from the browser's page cache: the mark is set again.
  passViewerShown(store, reloaded, false);
  passViewerShown(store, reloaded, true);
  assert.notEqual(passViewer(store.copy(), () => 'third').tab, reloaded.tab);
});

test('a blocked store still names the tab and page', () => {
  const blocked = { getItem() { throw new Error('blocked'); }, setItem() { throw new Error('blocked'); }, removeItem() { throw new Error('blocked'); } };
  const viewer = passViewer(blocked, ids());
  assert.equal(viewer.page, 'id-1');
  assert.equal(viewer.tab, 'id-1');
  passViewerShown(blocked, viewer, false);
  assert.deepEqual(passViewer(null, () => 'p'), { tab: 'p', page: 'p' });
});

test('a Pass after a Close waits for the stop, and each stop names its run and page', async () => {
  const sent = [];
  let answerStop;
  const send = (call, body) => {
    sent.push([call, body]);
    if (call === 'stop') return new Promise(resolve => { answerStop = resolve; });
    return Promise.resolve({ run: `run-${sent.length}`, state: 'opening' });
  };
  const line = passCalls(send, { tab: 't', page: 'p' });
  await line.start();
  assert.equal(line.run(), 'run-1');

  line.stop();
  assert.deepEqual(sent[1], ['stop', { run: 'run-1', tab: 't', page: 'p' }]);
  const starting = line.start();
  await new Promise(resolve => setTimeout(resolve, 5));
  assert.equal(sent.length, 2, 'the start waits for the stop to answer');
  answerStop({});
  await starting;
  assert.deepEqual(sent[2], ['start', { tab: 't', page: 'p' }]);
  assert.equal(line.run(), 'run-3');
});

test('a Close while the start is on its way names no run, and the late answer is not kept', async () => {
  const sent = [];
  let answerStart;
  const send = (call, body) => {
    sent.push([call, body]);
    if (call === 'start') return new Promise(resolve => { answerStart = resolve; });
    return Promise.resolve({});
  };
  const line = passCalls(send, { tab: 't', page: 'p' });
  const starting = line.start();
  await Promise.resolve();
  line.stop();
  assert.deepEqual(sent[1], ['stop', { run: null, tab: 't', page: 'p' }]);
  answerStart({ run: 'run-old', state: 'opening' });
  await starting;
  assert.equal(line.run(), null, "the run being stopped is not this page's any more");
});

test('a Close while a Pass waits for the stop before it ends that Pass before it is sent', async () => {
  const sent = [];
  let answerFirstStop;
  let stops = 0;
  const send = (call, body) => {
    sent.push([call, body]);
    if (call === 'stop' && ++stops === 1) return new Promise(resolve => { answerFirstStop = resolve; });
    return Promise.resolve({ run: 'run-new', state: 'opening' });
  };
  const line = passCalls(send, { tab: 't', page: 'p' });
  line.stop();
  const starting = line.start();
  line.stop();
  answerFirstStop({});
  const answer = await starting;
  assert.deepEqual(sent.map(([call]) => call), ['stop', 'stop'], 'no start goes out after the stops');
  assert.equal(answer.started, false);
  assert.equal(line.owesStop(), false);
});

test('a start whose answer never came still owes its stop, which names no run', async () => {
  const sent = [];
  let fail = false;
  const send = (call, body) => {
    sent.push([call, body]);
    if (call === 'start' && fail) return Promise.reject(new Error('the network dropped'));
    return Promise.resolve({ run: 'run-1', state: 'live' });
  };
  const line = passCalls(send, { tab: 't', page: 'p' });
  assert.equal(line.owesStop(), false, 'nothing started yet');
  await line.start();
  assert.equal(line.run(), 'run-1');
  // Try again after run-1 ended: Octo may have seated the page on a new run whose name never came.
  fail = true;
  await assert.rejects(line.start());
  assert.equal(line.owesStop(), true);
  line.stop();
  assert.deepEqual(sent.at(-1), ['stop', { run: null, tab: 't', page: 'p' }]);
  assert.equal(line.owesStop(), false);
});

test('leaving clears the tab mark and closes an open panel; coming back from the page cache takes a tab of its own', () => {
  const store = memoryStore();
  const viewer = passViewer(store, ids());
  const target = new EventTarget();
  let open = true;
  let closes = 0;
  let n = 0;
  passLifecycle(target, { store, viewer, makeId: () => `new-${++n}`, isOpen: () => open, close: () => { closes += 1; } });
  const before = viewer.tab;

  target.dispatchEvent(new Event('pagehide'));
  assert.equal(store.getItem('octo.passTabOpen'), null, 'a reload now finds the same tab');
  assert.equal(closes, 1);
  open = false;
  target.dispatchEvent(new Event('pagehide'));
  assert.equal(closes, 1, 'a closed panel sends nothing');

  // A fresh load's pageshow changes nothing.
  target.dispatchEvent(new Event('pageshow'));
  assert.equal(viewer.tab, before);

  const back = new Event('pageshow');
  Object.defineProperty(back, 'persisted', { value: true });
  target.dispatchEvent(back);
  assert.equal(viewer.tab, 'new-1');
  assert.equal(store.getItem('octo.passTab'), 'new-1');
  assert.equal(store.getItem('octo.passTabOpen'), 'new-1');
});

test("a leaving page's stop goes by the leaving sender, before stop returns", () => {
  const normal = [];
  const leaving = [];
  const line = passCalls((call, body) => { normal.push(call); return Promise.resolve({}); }, { tab: 't', page: 'p' });
  line.stop((call, body) => { leaving.push([call, body]); return Promise.resolve(); });
  assert.deepEqual(leaving, [['stop', { run: null, tab: 't', page: 'p' }]]);
  assert.deepEqual(normal, []);
  // A sender that throws is a stop that failed, never an error out of stop.
  assert.doesNotThrow(() => line.stop(() => { throw new Error('gone'); }));
});

test("the heart source step counts slskd or Mornye only when set up, answering and not warning", () => {
  const settings = { Subsonic: { HeartDownloadSources: [{ Source: 'Soulseek', SongEnabled: true }] } };
  const check = services => heartSourceReachable({ services }, settings);
  const off = { ok: true, configured: false, warning: false };
  const up = { ok: true, configured: true, warning: false };
  assert.equal(check({ slskd: off, mornye: off }).ok, false, 'an slskd nobody set up is not answering');
  assert.equal(check({ slskd: up, mornye: off }).detail, 'Soulseek answering.');
  assert.equal(check({ slskd: { ok: false }, mornye: up }).detail, 'Mornye answering.');
  assert.equal(check({ slskd: { ok: false }, mornye: { ...up, warning: true } }).ok, false, 'a paused Mornye is not answering');
  assert.equal(check({ slskd: up, mornye: up }).detail, 'Soulseek, Mornye answering.');
});
