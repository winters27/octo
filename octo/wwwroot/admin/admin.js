// Octo admin UI. Vanilla JS, no build step.

// Every call to Octo's admin API goes through here. Writes carry X-Octo-Admin, which a page on
// another origin cannot add without a preflight Octo refuses. A 401 that asks for a sign-in shows
// the sign-in screen, and the call is sent again once someone has signed in.
async function api(url, options = {}) {
  const method = (options.method || 'GET').toUpperCase();
  const send = () => {
    const headers = new Headers(options.headers || {});
    if (method !== 'GET' && method !== 'HEAD') headers.set('X-Octo-Admin', '1');
    return fetch(url, { credentials: 'same-origin', ...options, headers });
  };
  // A write may have started work somewhere (a scan, a re-tag, a download), so the activity
  // cards look again soon instead of at their idle pace.
  const kick = () => {
    if (method !== 'GET' && method !== 'HEAD' && typeof activityKick === 'function') activityKick();
  };
  const r = await send();
  // The activity cards ask in the background: their 401 never puts the sign-in screen up.
  if (r.status !== 401 || String(url).startsWith('/api/admin/activity')) {
    kick();
    return r;
  }
  const body = await r.clone().json().catch(() => ({}));
  if (!body.signIn) return r;
  await signInGate();
  const again = await send();
  kick();
  return again;
}

// The sign-in screen. However many calls ask at once there is one screen, and they all wait on
// the same promise. Navidrome admin by default; the recovery code for when Navidrome cannot vouch.
let gatePromise = null;
function signInGate() {
  if (gatePromise) return gatePromise;
  gatePromise = new Promise(resolve => {
    const gate = document.getElementById('signin-gate');
    const app = document.querySelector('.app');
    const user = document.getElementById('gate-user');
    const pass = document.getElementById('gate-pass');
    const code = document.getElementById('gate-code');
    const err = document.getElementById('gate-error');
    const toggle = document.getElementById('gate-use-code');
    let useCode = false;
    const show = () => {
      document.getElementById('gate-navidrome').hidden = useCode;
      document.getElementById('gate-recovery').hidden = !useCode;
      toggle.textContent = useCode ? 'Sign in with Navidrome instead' : 'Use the recovery code';
      (useCode ? code : user).focus();
    };
    if (app) app.inert = true;
    gate.hidden = false;
    err.textContent = '';
    show();
    toggle.onclick = () => { useCode = !useCode; err.textContent = ''; show(); };
    document.getElementById('gate-form').onsubmit = async event => {
      event.preventDefault();
      err.textContent = '';
      const [path, payload] = useCode
        ? ['/api/admin/auth/recovery', { code: code.value.trim() }]
        : ['/api/admin/browse/auth', { username: user.value.trim(), password: pass.value }];
      if (useCode ? !payload.code : !(payload.username && payload.password)) {
        err.textContent = useCode ? 'Type the recovery code.' : 'Both a username and a password are required.';
        return;
      }
      const r = await fetch(path, {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', 'X-Octo-Admin': '1' },
        body: JSON.stringify(payload),
      }).catch(() => null);
      const data = r ? await r.json().catch(() => ({})) : {};
      if (!r || !r.ok) { err.textContent = data.error || 'Octo did not answer.'; return; }
      // A browser that refuses the cookie would loop back to this screen on every call; say why instead.
      const kept = await fetch('/api/admin/browse/session', { credentials: 'same-origin' }).then(x => x.json()).catch(() => ({}));
      if (!kept.signedIn) {
        err.textContent = 'Signed in, but this browser did not keep the sign-in. Allow cookies for this address, and open Octo by the same address each time.';
        return;
      }
      pass.value = '';
      code.value = '';
      gate.hidden = true;
      if (app) app.inert = false;
      showSignedIn(data.user, data.recovery);
      gatePromise = null;
      resolve();
    };
  });
  return gatePromise;
}

// Asked once at load: already signed in, sign-in switched off, or the sign-in screen first.
async function ensureSignedIn() {
  let body = {};
  try { body = await (await fetch('/api/admin/browse/session', { credentials: 'same-origin' })).json(); } catch { }
  document.getElementById('signin-off-banner').hidden = !body.signInOff;
  if (body.signedIn) { showSignedIn(body.user, body.recovery); return; }
  if (body.signInOff) return;
  await signInGate();
}
const ready = ensureSignedIn();

// Every icon is one symbol in icons.svg: Phosphor glyphs as i-name, brand marks as b-name. The
// sprite is put in the page itself, because a <use> pointing into another file cannot paint a
// brand's gradient in every browser. Until it arrives an icon is an empty box of the right size.
fetch('/admin/icons.svg')
  .then(r => (r.ok ? r.text() : ''))
  .then(svg => { if (svg) document.body.insertAdjacentHTML('afterbegin', svg); })
  .catch(() => {});

function icon(name, extraClass = '') {
  const cls = name.startsWith('b-') ? 'icon brand' : 'icon';
  return `<svg class="${cls}${extraClass ? ` ${extraClass}` : ''}" aria-hidden="true"><use href="#${name}"/></svg>`;
}

// ────────────────────────────────────────────────────────────────
// Sidebar nav: tab switching
// ────────────────────────────────────────────────────────────────
const navItems = document.querySelectorAll('.sidebar-nav-item');
const panes    = document.querySelectorAll('section[data-pane]');

// focus: true when the user chose the pane, so keyboard and screen-reader focus follows them
// to its heading instead of being left on a sidebar button that no longer describes the page.
function activateTab(name, { focus = false } = {}) {
  navItems.forEach(b => {
    const on = b.dataset.tab === name;
    b.classList.toggle('active', on);
    if (on) b.setAttribute('aria-current', 'page');
    else b.removeAttribute('aria-current');
    // On a phone the nav is a sideways strip; keep the current pill in view.
    if (on && window.matchMedia('(max-width: 760px)').matches) {
      b.scrollIntoView({ block: 'nearest', inline: 'center' });
    }
  });
  panes.forEach(p => p.classList.toggle('active', p.dataset.pane === name));
  if (typeof activityTick === 'function' && activityCards) activityTick();
  if (location.hash !== `#${name}`) history.replaceState(null, '', `#${name}`);
  // Segmented thumbs can only be measured once their pane is visible.
  if (typeof syncSegments === 'function') syncSegments(false);
  // Panes that show live data reload whenever they are opened, so they are never stale.
  if (name === 'fetched' && typeof loadFetched === 'function') loadFetched();
  if ((name === 'status' || name === 'fetched') && typeof loadAmbient === 'function') loadAmbient();
  if (name === 'lossy' && typeof loadLossy === 'function') loadLossy();
  if (name === 'health' && typeof loadHealth === 'function') loadHealth();
  if (name === 'imports' && typeof loadImports === 'function') loadImports();
  if (name === 'raw' && typeof loadRawConfig === 'function') loadRawConfig();
  if (name === 'sources' && typeof loadConfigSources === 'function') loadConfigSources();
  if (name === 'lastfm' && typeof loadRadioStatus === 'function') loadRadioStatus();
  if (name === 'lastfm' && typeof loadSonic === 'function') loadSonic();
  if (name === 'lastfm' && typeof loadRadioOutcomes === 'function') loadRadioOutcomes();
  if (name === 'lastfm' && typeof loadLastFmScrobbling === 'function') loadLastFmScrobbling();
  if (name === 'lastfm' && typeof loadLastFmAccount === 'function') loadLastFmAccount();
  if (name === 'about' && typeof loadUpdate === 'function') loadUpdate();
  if (name === 'soulseek' && typeof loadSharing === 'function') loadSharing();
  if (focus) {
    window.scrollTo({ top: 0 });
    const heading = document.querySelector(`section[data-pane="${name}"] h1`);
    if (heading) {
      heading.setAttribute('tabindex', '-1');
      heading.focus({ preventScroll: true });
    }
  }
}

// A pane change is a history entry, so Back returns to the previous pane.
function openTab(name) {
  if (!document.querySelector(`section[data-pane="${name}"]`)) return;
  if (location.hash !== `#${name}`) history.pushState(null, '', `#${name}`);
  activateTab(name, { focus: true });
}

navItems.forEach(btn => btn.addEventListener('click', () => openTab(btn.dataset.tab)));

// Back/forward, and a typed or linked #pane. Both events can fire for one change, and
// activating the active pane again is harmless.
function followHash() {
  const target = location.hash.slice(1) || 'status';
  const pane = document.querySelector(`section[data-pane="${target}"]`);
  if (pane && !pane.classList.contains('active')) activateTab(target);
}
window.addEventListener('popstate', followHash);
window.addEventListener('hashchange', followHash);
// The #pane in the address on load is honoured at Boot, once every pane's loader exists.

// ────────────────────────────────────────────────────────────────
// Activity: live progress of Octo's background work
// ────────────────────────────────────────────────────────────────
// One card per job (a re-tag, a covers or lyrics run, better copies, hearted downloads, a
// Spotify read, a duplicate scan, an slskd rescan, Navidrome's scan), updated in place from
// GET /api/admin/activity, on whatever page is open. A card never repeats: a job is one card
// for as long as it runs, then says how it ended and goes. The page that shows a job in full
// hides its card, and is refreshed when that job starts or ends, so it is never left stale.
// Asked every 2.5 s while something runs, every 20 s otherwise, and not while the tab is hidden.
const ACTIVITY_FAST = 2500;
const ACTIVITY_IDLE = 20000;
const ACTIVITY_LINGER = 6000;   // a finished card stays this long; a failed one until dismissed
// Navidrome rescans for a few seconds after every download; only a longer scan earns a card.
const ACTIVITY_SCAN_GRACE = 5000;
const activityCards = document.getElementById('activity-cards');
const activitySay = document.getElementById('activity-say');
const activityOpenedAt = Date.now();
const activitySeen = new Map();      // key -> { id, state, firstRunningAt, json }
const activityShownEnd = new Set();  // finished item ids already shown
const activityDismissed = new Map(); // key -> item id dismissed
let activityTimer = null;
let activityFastUntil = 0;

// What reloads a page when its job starts or ends.
const ACTIVITY_PAGE_REFRESH = {
  tags: () => typeof loadGenreBackfill === 'function' && loadGenreBackfill(),
  covers: () => typeof loadCoverUpgrade === 'function' && loadCoverUpgrade(),
  lyrics: () => typeof loadLyricsLibrary === 'function' && loadLyricsLibrary(),
  lossy: () => typeof loadUpgrades === 'function' && loadUpgrades(),
  health: () => typeof loadHealth === 'function' && loadHealth(),
  fetched: () => typeof loadFetched === 'function' && loadFetched(),
  imports: () => typeof loadImports === 'function' && loadImports(),
  soulseek: () => typeof loadSharing === 'function' && loadSharing(),
};

// One card per job kind, so a run that becomes the next run, or a running batch that becomes
// "Added 3 songs", stays the same card. A Spotify read and its trickle are two jobs.
function activityKey(item) {
  return item.kind === 'imports' ? item.id.split(':').slice(0, 2).join(':') : item.kind;
}

function activityPageOpen(page) {
  return document.querySelector(`section[data-pane="${page}"]`)?.classList.contains('active') ?? false;
}

// The card's mark is the icon its page has in the sidebar, so a card reads as that page.
function activityIcon(item) {
  if (item.kind === 'scan') return icon('b-navidrome');
  const use = document.querySelector(`.sidebar-nav-item[data-tab="${item.page}"] use`);
  return icon(use?.getAttribute('href')?.replace('#', '') || 'i-pulse');
}

function activityFraction(item) {
  if (typeof item.fraction === 'number') return Math.max(0, Math.min(1, item.fraction));
  if (item.total > 0 && typeof item.done === 'number') return Math.max(0, Math.min(1, item.done / item.total));
  return null;
}

function activityLeft(item, fraction) {
  if (item.state !== 'running' || fraction === null || fraction < 0.04 || !item.startedUtc) return '';
  const elapsed = (Date.now() - Date.parse(item.startedUtc)) / 1000;
  if (!(elapsed > 15)) return '';
  const left = elapsed * (1 - fraction) / fraction;
  if (left < 45) return 'under a minute left';
  if (left < 3600) return `about ${Math.round(left / 60)} min left`;
  return `about ${Math.round(left / 3600)} h left`;
}

function activityLine(item) {
  const fraction = activityFraction(item);
  const parts = [];
  if (item.state === 'running' && item.total > 0 && typeof item.done === 'number')
    parts.push(`${item.done.toLocaleString()} of ${item.total.toLocaleString()}${item.unit ? ' ' + item.unit : ''}`);
  else if (item.state === 'running' && typeof item.done === 'number' && item.unit)
    parts.push(`${item.done.toLocaleString()} ${item.unit}`);
  if (item.detail) parts.push(item.detail);
  else if (item.state === 'failed' && item.error) parts.push(item.error);
  const left = activityLeft(item, fraction);
  if (left) parts.push(left);
  return parts.join(' · ');
}

function activityCardHtml(item) {
  const fraction = activityFraction(item);
  const ended = item.state !== 'running';
  const mark = item.state === 'done' ? icon('i-check-circle')
    : item.state === 'failed' ? icon('i-warning-circle')
    : activityIcon(item);
  const bar = ended ? ''
    : fraction === null
      ? '<div class="act-bar indeterminate" aria-hidden="true"><span></span></div>'
      : `<div class="act-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${Math.round(fraction * 100)}"><span style="width:${(fraction * 100).toFixed(1)}%"></span></div>`;
  const pct = !ended && fraction !== null ? `<span class="act-pct">${Math.round(fraction * 100)}%</span>` : '';
  return `<button type="button" class="act-body" data-act-open="${escapeHtml(item.page)}" aria-label="${escapeHtml(item.title)}. Open its page.">
      <span class="act-mark">${mark}</span>
      <span class="act-text"><span class="act-title">${escapeHtml(item.title)}</span><span class="act-line">${escapeHtml(activityLine(item))}</span></span>
      ${pct}
    </button>
    <button type="button" class="act-close" data-act-close aria-label="Dismiss"><svg class="icon" aria-hidden="true"><use href="#i-x"/></svg></button>
    ${bar}`;
}

function activityRemove(key) {
  const el = activityCards?.querySelector(`[data-act-key="${CSS.escape(key)}"]`);
  if (!el) return;
  el.classList.add('leaving');
  setTimeout(() => el.remove(), 220);
}

function renderActivity(items) {
  if (!activityCards) return;
  const now = Date.now();
  const live = new Set();

  for (const item of items) {
    const key = activityKey(item);
    const seen = activitySeen.get(key);
    const running = item.state === 'running';
    const json = JSON.stringify(item);
    const changedState = !seen || seen.id !== item.id || seen.state !== item.state;

    // The page that shows this job in full is refreshed when it starts or ends.
    if (changedState && activityPageOpen(item.page)) ACTIVITY_PAGE_REFRESH[item.page]?.();

    activitySeen.set(key, {
      id: item.id,
      state: item.state,
      firstRunningAt: running ? (seen?.state === 'running' && seen.id === item.id ? seen.firstRunningAt : now) : seen?.firstRunningAt,
      sawRunning: running || (seen?.sawRunning ?? false),
      page: item.page,
      json,
    });

    // A finished job earns a card once, and only if this page saw it run or it ended after the
    // page was opened; a job that ended before you came is not news.
    if (!running) {
      const finished = item.finishedUtc ? Date.parse(item.finishedUtc) : NaN;
      const fresh = (seen?.sawRunning ?? false) || (Number.isFinite(finished) && finished >= activityOpenedAt);
      if (!fresh || activityShownEnd.has(item.id)) continue;
    }
    if (running && item.kind === 'scan' && now - activitySeen.get(key).firstRunningAt < ACTIVITY_SCAN_GRACE) continue;
    if (activityDismissed.get(key) === item.id) continue;
    if (activityPageOpen(item.page)) continue;

    live.add(key);
    let el = activityCards.querySelector(`[data-act-key="${CSS.escape(key)}"]`);
    if (!el) {
      el = document.createElement('div');
      el.className = 'act';
      el.dataset.actKey = key;
      activityCards.appendChild(el);
    }
    if (el.dataset.json !== json) {
      el.dataset.json = json;
      el.dataset.state = item.state;
      el.dataset.itemId = item.id;
      el.innerHTML = activityCardHtml(item);
    }
    if (!running && !activityShownEnd.has(item.id)) {
      activityShownEnd.add(item.id);
      if (activitySay) activitySay.textContent = `${item.title}. ${activityLine(item)}`;
      if (item.state !== 'failed') setTimeout(() => {
        if (activityCards.querySelector(`[data-act-key="${CSS.escape(key)}"]`)?.dataset.itemId === item.id) activityRemove(key);
      }, ACTIVITY_LINGER);
    }
  }

  // A job that is no longer listed: its card goes, unless it is showing how it ended.
  for (const el of [...activityCards.children]) {
    if (live.has(el.dataset.actKey)) continue;
    if (el.dataset.state !== 'running' && !el.classList.contains('leaving')) continue;
    activityRemove(el.dataset.actKey);
  }
  for (const [key, seen] of activitySeen) {
    if (items.some(item => activityKey(item) === key)) continue;
    // Gone from the list: whatever it was is over. A page showing it catches up once.
    activitySeen.delete(key);
    activityDismissed.delete(key);
    if (seen.page && activityPageOpen(seen.page)) ACTIVITY_PAGE_REFRESH[seen.page]?.();
  }
}

async function activityTick() {
  clearTimeout(activityTimer);
  activityTimer = null;
  let items = [];
  try {
    const r = await api('/api/admin/activity', { cache: 'no-store' });
    if (r.ok) items = (await r.json()).items || [];
  } catch { /* Octo away: the status cards say so */ }
  renderActivity(items);
  if (document.hidden) return;
  const busy = items.some(item => item.state === 'running') || Date.now() < activityFastUntil;
  activityTimer = setTimeout(activityTick, busy ? ACTIVITY_FAST : ACTIVITY_IDLE);
}

// Something may have just started: look again in a moment, and keep looking quickly a while.
function activityKick() {
  activityFastUntil = Date.now() + 20000;
  clearTimeout(activityTimer);
  activityTimer = setTimeout(activityTick, 700);
}

activityCards?.addEventListener('click', e => {
  const close = e.target.closest('[data-act-close]');
  const card = e.target.closest('.act');
  if (!card) return;
  if (close) {
    activityDismissed.set(card.dataset.actKey, card.dataset.itemId);
    activityRemove(card.dataset.actKey);
    return;
  }
  const page = e.target.closest('[data-act-open]')?.dataset.actOpen;
  if (page && document.querySelector(`section[data-pane="${page}"]`)) {
    openTab(page);
    activityRemove(card.dataset.actKey);
  }
});
document.addEventListener('visibilitychange', () => { if (!document.hidden) activityTick(); });

// ────────────────────────────────────────────────────────────────
// Toast helper
// ────────────────────────────────────────────────────────────────
const toastEl = document.getElementById('toast');
let toastTimer = null;
const srAlert = document.getElementById('sr-alert');
function toast(msg, kind = 'ok') {
  const tone = kind === 'err' ? 'error' : kind;
  toastEl.textContent = msg;
  toastEl.className = `show ${tone}`;
  clearTimeout(toastTimer);
  // An error stays long enough to read, and is also put in an assertive region: changing the
  // polite toast's role on the fly is not reliably announced.
  toastTimer = setTimeout(() => { toastEl.className = ''; }, tone === 'error' ? 8000 : 3500);
  if (tone === 'error' && srAlert) {
    srAlert.textContent = '';
    setTimeout(() => { srAlert.textContent = msg; }, 50);
  }
}
toastEl.addEventListener('click', () => { toastEl.className = ''; });

// The toast is for news from elsewhere, like Octo coming back after a restart. What a button did
// is said beside that button, where the eye already is, and stays until the next thing happens
// there. An error is also put in the assertive region, so a screen reader hears it.
const noteTimers = new WeakMap();
function note(anchor, message, kind = 'ok') {
  if (!anchor) return;
  let holder = anchor.nextElementSibling?.classList.contains('inline-note') ? anchor.nextElementSibling : null;
  if (!holder) {
    holder = document.createElement('span');
    holder.className = 'inline-note';
    holder.setAttribute('role', 'status');
    anchor.after(holder);
  }
  clearTimeout(noteTimers.get(holder));
  if (!message) { holder.hidden = true; return; }
  const glyph = { ok: 'i-check-circle-fill', error: 'i-x-circle-fill', info: 'i-info-fill', busy: 'i-circle-notch' }[kind] ?? 'i-info-fill';
  holder.className = `inline-note ${kind}`;
  holder.innerHTML = `${icon(glyph)}<span>${esc(message)}</span>`;
  holder.hidden = false;
  if (kind === 'error' && srAlert) {
    srAlert.textContent = '';
    setTimeout(() => { srAlert.textContent = message; }, 50);
  }
  // Good news fades after a while; a problem stays until it is dealt with.
  if (kind === 'ok') noteTimers.set(holder, setTimeout(() => { holder.hidden = true; }, 8000));
}

// Loading, empty and failed lists all look the same everywhere: an icon and a sentence.
function stateBlock(kind, message) {
  const glyph = { loading: 'i-circle-notch', empty: 'i-circle-dashed', error: 'i-warning-circle' }[kind] ?? 'i-info';
  return `<div class="state state-${kind}"${kind === 'error' ? ' role="alert"' : ''}>${icon(glyph)}<span>${esc(message)}</span></div>`;
}

// ────────────────────────────────────────────────────────────────
// Status grid + sidebar badge
// ────────────────────────────────────────────────────────────────
const statusBadge      = document.querySelector('[data-status-badge]');
const statusLastChecked = document.getElementById('status-last-checked');

async function refreshStatus() {
  try {
    const r = await api('/api/admin/status');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();

    setStatusCard('octo', data.octo);
    Object.entries(data.services || {}).forEach(([k, v]) => setStatusCard(k, v));

    // Update sidebar badge with bad count, if any. An optional service nobody set up reports
    // ok, so it never counts here.
    const all = [data.octo, ...Object.values(data.services || {})];
    const badCount = all.filter(s => !s?.ok).length;
    setStatusBadge(badCount > 0 ? String(badCount) : '',
      `${badCount} service${badCount === 1 ? '' : 's'} need${badCount === 1 ? 's' : ''} attention`);
    if (statusLastChecked) {
      statusLastChecked.textContent = new Date().toLocaleTimeString();
    }
    lastStatus = data;
    renderSetupChecklist();
  } catch (e) {
    document.querySelectorAll('.status-card').forEach(card => {
      card.classList.add('bad');
      card.classList.remove('off');
      const dot = card.querySelector('.status-dot');
      const body = card.querySelector('.status-card-body');
      if (dot) dot.className = 'status-dot bad';
      if (body) body.textContent = `Octo did not answer: ${e.message}`;
    });
    setStatusBadge('!', 'Octo did not answer the status check');
  }
}

function setStatusBadge(text, label) {
  if (!statusBadge) return;
  statusBadge.className = text ? 'sidebar-nav-badge bad' : 'sidebar-nav-badge';
  statusBadge.textContent = text;
  if (text) statusBadge.setAttribute('aria-label', label);
  else statusBadge.removeAttribute('aria-label');
}

function setStatusCard(svc, probe) {
  const card = document.querySelector(`.status-card[data-svc="${svc}"]`);
  if (!card) return;
  const state = probe.warning ? 'warn' : probe.configured === false ? 'off' : probe.ok ? 'ok' : 'bad';
  card.classList.toggle('bad', state === 'bad');
  card.classList.toggle('warn', state === 'warn');
  card.classList.toggle('off', state === 'off');
  const dot = card.querySelector('.status-dot');
  const body = card.querySelector('.status-card-body');
  if (dot)  dot.className  = `status-dot ${state}`;
  if (body) body.textContent = probe.detail || (probe.ok ? 'online' : 'unreachable');
}

document.getElementById('status-refresh')?.addEventListener('click', refreshStatus);
ready.then(refreshStatus);
// No point probing every service while nobody is looking; catch up when the tab returns.
setInterval(() => { if (!document.hidden) refreshStatus(); }, 30_000);
document.addEventListener('visibilitychange', () => { if (!document.hidden) refreshStatus(); });

// ────────────────────────────────────────────────────────────────
// Settings: load -> populate forms -> save on submit
// ────────────────────────────────────────────────────────────────
let currentSettings = null;
let lastStatus = null;
let lastLibraryStatus = null;

// Without the saved values every form still holds its HTML defaults, and saving one would write
// blanks and false over real settings. So when they cannot be loaded, saving is switched off.
function setSettingsAvailable(ok, reason = '') {
  const banner = document.getElementById('settings-unavailable');
  if (banner) {
    banner.hidden = ok;
    const msg = banner.querySelector('.msg');
    if (msg) msg.textContent = ok ? '' : `Octo could not load its settings (${reason}).`;
  }
  document.querySelectorAll('form[data-section] button[type="submit"], #raw-form button[type="submit"]')
    .forEach(button => { button.disabled = !ok; });
}
document.getElementById('settings-retry')?.addEventListener('click', () => loadSettings());

async function loadSettings() {
  let r;
  try {
    r = await api('/api/admin/settings', { cache: 'no-store' });
  } catch (e) {
    setSettingsAvailable(false, e.message || 'no answer');
    return;
  }
  if (!r.ok) {
    setSettingsAvailable(false, `HTTP ${r.status}`);
    return;
  }
  currentSettings = await r.json();
  setSettingsAvailable(true);
  const invalid = document.getElementById('config-invalid-banner');
  if (invalid) invalid.hidden = currentSettings?._meta?.ConfigFileValid !== false;

  // Populate every <input>/<select> whose name="Section.Key" matches.
  document.querySelectorAll('[name]').forEach(el => {
    if (!el.name?.includes('.')) return;
    const [section, key] = el.name.split('.');
    const value = currentSettings?.[section]?.[key];
    if (value === undefined || value === null) return;
    if (el.dataset.json === 'true') el.value = JSON.stringify(value ?? []);
    else if (el.type === 'checkbox') el.checked = !!value;
    else el.value = value;
  });

  syncPlaybackSourceControl();
  updateStreamSettings();
  updateRadioPublicationSettings();
  renderHeartSourceOrder(currentSettings?.Subsonic?.HeartDownloadSources);
  renderLyricsSources(currentSettings?.Metadata?.LyricsSources ?? '');
  renderRadioDiscovery(currentSettings?.LastFm?.DiscoveryStations);
  renderRejectedPeerCount();
  renderGenreMappings(currentSettings?.Genre?.Mappings);
  renderGenreBlocklist(currentSettings?.Genre?.Blocklist);
  syncGenreUnknownRow();
  renderLibraryActions(currentSettings?.LibraryActions?.Actions);
  renderLibraryActionUsers(currentSettings?.LibraryActions?.AllowedUsers);
  // retry: false, so a page load never pops a sign-in prompt. Without a session the section
  // simply stays empty until the user asks for a preview.
  loadGenreBackfill();
  loadExplicitBackfill();
  loadCoverUpgrade();
  loadLyricsLibrary();
  loadLyricsChoices();
  loadRadioStatus();
  loadQualityUpgrade();
  // Again here, so saving a songs-an-hour value enables its Pause button straight away.
  loadReviewSweep();
  loadLastFmScrobbling();
  // Only when it is the tab on screen; opening the tab later checks then.
  if (document.querySelector('[data-pane="lastfm"].active')) loadLastFmAccount();

  // Meta references
  const cfgPath = document.getElementById('meta-config-path');
  if (cfgPath && currentSettings?._meta?.ConfigFilePath) {
    cfgPath.textContent = currentSettings._meta.ConfigFilePath;
  }
  const version = document.getElementById('meta-version');
  if (version && currentSettings?._meta?.Version) {
    version.textContent = currentSettings._meta.Version;
  }
  renderSlskdOpen();

  renderOctoAddresses();
  updateDiscoveryBanner();
  buildSegments();
  syncSegments(false);
  if (currentSettings?.Lidarr?.BaseUrl && currentSettings?.Lidarr?.ApiKey) {
    loadLidarrOptions();
  }

  bindSecretPlaceholders();
  renderRestartPending();
  renderSetupChecklist();

  // Everything above has filled the forms from what is saved, so this is what "unchanged" means.
  document.querySelectorAll('form[data-section]').forEach(form => {
    form.querySelector('.form-actions')?.classList.remove('dirty');
    markClean(form);
  });
}

// A saved admin password comes back as a placeholder. Selecting it on focus means typing
// replaces it; appending to it would save the placeholder plus the typing as the password.
function bindSecretPlaceholders() {
  const placeholder = currentSettings?._meta?.SecretPlaceholder;
  if (!placeholder) return;
  document.querySelectorAll('input[type="password"]').forEach(input => {
    if (input.dataset.placeholderBound) return;
    input.dataset.placeholderBound = '1';
    input.addEventListener('focus', () => { if (input.value === placeholder) input.select(); });
  });
}

// ── Restart pending ─────────────────────────────────────────────────────────
// Computed by Octo from what it started with, not remembered by this page, so it is right in
// every tab and after a reload, and it clears itself once Octo restarts.
function settingLabel(configKey) {
  const name = configKey.replace(':', '.');
  const field = document.querySelector(`[name="${name}"]`);
  if (field?.dataset.restartLabel) return field.dataset.restartLabel;
  const title = field?.closest('.set-row')?.querySelector('.set-info-t');
  if (!title) return configKey;
  const copy = title.cloneNode(true);
  copy.querySelectorAll('.restart-badge, .set-opt').forEach(el => el.remove());
  return copy.textContent.trim() || configKey;
}

function renderRestartPending() {
  const banner = document.getElementById('restart-pending');
  const restartButton = document.getElementById('restart-btn');
  const pending = currentSettings?._meta?.RestartPending ?? [];
  restartButton?.classList.toggle('needs-restart', pending.length > 0);
  if (!banner) return;
  banner.hidden = pending.length === 0;
  if (!pending.length) return;
  const labels = [...new Set(pending.map(settingLabel))];
  banner.innerHTML = `<span><strong>Restart Octo to apply:</strong> ${esc(labels.join(', '))}.</span>
    <button type="button" class="btn btn-ghost" id="restart-pending-now">Restart now</button>`;
  document.getElementById('restart-pending-now')
    ?.addEventListener('click', () => document.getElementById('restart-btn')?.click());
}

// ── Unsaved changes ─────────────────────────────────────────────────────────
// A form's fingerprint is what its visible controls hold. Heart rows carry their source name,
// because reordering them changes no value, only the order.
function formFingerprint(form) {
  return JSON.stringify(Array.from(form.querySelectorAll('input:not([type="hidden"]), select, textarea'))
    .map(el => `${el.closest('.source-priority-row')?.querySelector('.source-title')?.textContent ?? ''}|${el.name || el.id}|${el.type === 'checkbox' ? el.checked : el.value}`));
}

function markClean(form) {
  form.dataset.saved = formFingerprint(form);
  // What every control held when saved, for Undo changes.
  form._snapshot = Array.from(form.querySelectorAll('input, select, textarea'))
    .map(el => ({ el, value: el.value, checked: el.checked }));
  updateDirty(form);
}

// Puts a card back the way it was last saved. Lists drawn from a hidden JSON field (heart and
// lyrics sources, genre rules, pinned stations, library actions) are drawn again from it.
function revertForm(form) {
  (form._snapshot || []).forEach(({ el, value, checked }) => {
    if (!el.isConnected) return;
    if (el.type === 'checkbox') el.checked = checked;
    else el.value = value;
  });
  const redraw = {
    'f-heart-download-sources': value => renderHeartSourceOrder(JSON.parse(value || '[]')),
    'f-lyrics-sources': value => renderLyricsSources(value),
    'radio-discovery-json': value => renderRadioDiscovery(JSON.parse(value || '[]')),
    'genre-mappings-json': value => renderGenreMappings(JSON.parse(value || '[]')),
    'library-actions-json': value => renderLibraryActions(JSON.parse(value || '[]')),
  };
  Object.entries(redraw).forEach(([id, draw]) => {
    const field = form.querySelector(`#${id}`);
    if (field) { try { draw(field.value); } catch { /* keeps what is drawn */ } }
  });
  form.querySelectorAll('.field-error').forEach(el => { el.hidden = true; });
  syncPlaybackSourceControl();
  updateStreamSettings();
  updateRadioPublicationSettings();
  syncGenreUnknownRow();
  updateDiscoveryBanner();
  syncSegments(false);
  form.querySelector('.form-actions')?.classList.remove('dirty');
  // The card now holds exactly what was saved. A list drawn again from its tidied JSON can differ
  // from the rows on screen at save time (a trimmed space, a switched-off source moved last), so
  // this is the new picture of "unchanged", or the card would stay marked unsaved.
  form.dataset.saved = formFingerprint(form);
  updateDirty(form);
}

// For code that refills a form after load (Lidarr's choices, the library picker): that is not
// the user changing anything, so a form that was clean stays clean.
function markCleanIfWasClean(form, fill) {
  const wasClean = !form || !form.classList.contains('unsaved');
  fill();
  if (form && wasClean) markClean(form);
}

function updateDirty(form) {
  if (form.dataset.saved === undefined) return;
  const unsaved = formFingerprint(form) !== form.dataset.saved;
  form.classList.toggle('unsaved', unsaved);
  const actions = form.querySelector('.form-actions');
  actions?.classList.toggle('unsaved', unsaved);
  const status = actions?.querySelector('.unsaved-status');
  const statusText = unsaved ? 'Unsaved changes' : '';
  // Only when it changes: rewriting it swaps the text node, which the form's MutationObserver
  // would take as an edit and check again, every frame, and a live region would re-announce.
  if (status && status.textContent !== statusText) status.textContent = statusText;
  updateNavDirty();
}

function updateNavDirty() {
  navItems.forEach(item => {
    const pane = document.querySelector(`section[data-pane="${item.dataset.tab}"]`);
    const unsaved = !!pane?.querySelector('form.unsaved');
    item.classList.toggle('has-unsaved', unsaved);
    let hint = item.querySelector('.unsaved-hint');
    if (unsaved && !hint) {
      hint = document.createElement('span');
      hint.className = 'visually-hidden unsaved-hint';
      hint.textContent = ' (unsaved changes)';
      item.appendChild(hint);
    } else if (!unsaved && hint) {
      hint.remove();
    }
  });
}

window.addEventListener('beforeunload', event => {
  if (document.querySelector('form.unsaved') || rawDirty) {
    event.preventDefault();
    event.returnValue = '';
  }
});

// ────────────────────────────────────────────────────────────────
// Inject Save bar into every settings card
// ────────────────────────────────────────────────────────────────
function ensureSaveBar(form) {
  if (form.querySelector('.form-actions')) return;
  const actions = document.createElement('div');
  actions.className = 'form-actions';
  actions.innerHTML = `
    <button type="submit" class="btn btn-primary" ${currentSettings ? '' : 'disabled'} title="Save (Ctrl+S)">Save</button>
    <button type="button" class="btn btn-ghost form-undo">${icon('i-arrow-counter-clockwise')}<span>Undo changes</span></button>
    ${form.id === 'lidarr-connection-form' ? `<button type="button" class="btn btn-ghost" id="lidarr-test-connection">${icon('i-plugs-connected')}<span>Test connection</span></button>` : ''}
    <span class="unsaved-status" aria-live="polite"></span>
    <span class="saved-status" role="status"></span>
    <span class="restart-hint">
      ${icon('i-arrow-clockwise')}
      Restart required for one or more changes
    </span>
  `;
  form.appendChild(actions);
  actions.querySelector('.form-undo').addEventListener('click', () => {
    revertForm(form);
    actions.querySelector('button[type="submit"]')?.focus();
  });
}

// Ctrl+S (Cmd+S on a Mac) saves the card being edited: the one holding the keyboard, or else
// the only card on this page with unsaved changes. Never the browser's own "save page".
document.addEventListener('keydown', event => {
  if (!(event.ctrlKey || event.metaKey) || event.altKey || event.key.toLowerCase() !== 's') return;
  event.preventDefault();
  if (document.querySelector('.modal-backdrop:not([hidden])')) return;
  const pane = document.querySelector('section[data-pane].active');
  let form = document.activeElement?.closest('form[data-section], #raw-form');
  if (!form) {
    const unsaved = pane ? Array.from(pane.querySelectorAll('form[data-section].unsaved')) : [];
    if (unsaved.length === 1) form = unsaved[0];
    else if (pane?.dataset.pane === 'raw' && rawDirty) form = rawForm;
  }
  const submit = form?.querySelector('button[type="submit"]');
  if (form && submit && !submit.disabled) form.requestSubmit(submit);
});

// What a save said, in the card's own bar: a tick and the time, or what went wrong.
function saveStatus(form, message, kind = 'ok') {
  const status = form?.querySelector('.saved-status');
  if (!status) return;
  status.className = `saved-status ${kind}`;
  status.innerHTML = message
    ? `${icon(kind === 'error' ? 'i-x-circle-fill' : kind === 'busy' ? 'i-circle-notch' : 'i-check-circle-fill')}<span>${esc(message)}</span>`
    : '';
  if (kind === 'error' && srAlert) {
    srAlert.textContent = '';
    setTimeout(() => { srAlert.textContent = message; }, 50);
  }
}

// Builds the settings patch a form saves. A JSON field that does not parse stops the save
// rather than being sent as an empty list: that used to wipe every ListenBrainz per-user token
// on a single typo.
function collectPatch(form) {
  const patch = {};
  let needsRestart = false;
  let invalid = null;

  form.querySelectorAll('[name]').forEach(el => {
    if (invalid || !el.name?.includes('.')) return;
    if (el.disabled) return;
    // A select nobody could choose from has nothing to say; saving "" would clear a setting.
    if (el.tagName === 'SELECT' && el.options.length === 0) return;
    const [section, key] = el.name.split('.');
    patch[section] = patch[section] || {};
    let value;
    if (el.dataset.json === 'true') {
      try {
        value = JSON.parse(el.value || el.dataset.jsonEmpty || '[]');
        if (el.dataset.jsonEmpty === '{}'
            && (value === null || typeof value !== 'object' || Array.isArray(value))) {
          throw new Error('expected an object');
        }
      } catch {
        invalid = el;
        return;
      }
    } else if (el.type === 'checkbox') value = el.checked;
    else if (el.type === 'number' || el.dataset.number === 'true') {
      value = el.value === '' ? null : Number(el.value);
      if (Number.isNaN(value)) value = null;
    } else value = el.value;
    patch[section][key] = value;

    if (el.dataset.restart === 'true' && isFieldDirty(el)) {
      needsRestart = true;
    }
  });

  return { patch, needsRestart, invalid };
}

document.querySelectorAll('form[data-section]').forEach(form => {
  ensureSaveBar(form);

  // Restart hint: shown while a restart-required field differs from what is saved.
  // Unsaved changes: re-checked on every edit, and on row changes that fire no input event
  // (drag to reorder, a preset loaded, a rule removed), coalesced to one check per frame.
  let pendingCheck = false;
  const scheduleDirtyCheck = () => {
    if (pendingCheck) return;
    pendingCheck = true;
    requestAnimationFrame(() => {
      pendingCheck = false;
      const dirty = Array.from(form.querySelectorAll('[data-restart="true"]'))
        .some(el => isFieldDirty(el));
      form.querySelector('.form-actions')?.classList.toggle('dirty', dirty);
      updateDirty(form);
    });
  };
  form.addEventListener('input', scheduleDirtyCheck);
  form.addEventListener('change', scheduleDirtyCheck);
  new MutationObserver(scheduleDirtyCheck).observe(form, { childList: true, subtree: true });

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (form.id === 'radio-discovery-form' && !syncRadioDiscoveryInput()) return;
    if (form.id === 'genre-form' && !syncGenreMappingsInput()) return;
    if (form.id === 'library-actions-list-form' && !syncLibraryActionsInput()) return;
    if (form.id === 'library-actions-form') {
      const dry = form.querySelector('[name="LibraryActions.DryRun"]');
      if (currentSettings?.LibraryActions?.DryRun && dry && !dry.checked
          && !(await askConfirm('Turn off rehearsal mode?', 'From the next check, a track added to an action playlist, or rated if ratings are on, moves a real file into quarantine.', 'Turn off rehearsal', true))) return;
    }

    if (form.id === 'lastfm-account-form' && !(await lastFmAccountMaySave(form))) return;

    const { patch, needsRestart, invalid } = collectPatch(form);
    if (invalid) {
      const holder = invalid.dataset.errorFor ? document.getElementById(invalid.dataset.errorFor) : null;
      const message = "This isn't valid JSON, so nothing was saved.";
      if (holder) {
        holder.textContent = message;
        holder.hidden = false;
      }
      saveStatus(form, message, 'error');
      // A field folded into "More settings" is opened, so the error is where focus lands.
      invalid.closest('details')?.setAttribute('open', '');
      invalid.focus();
      return;
    }
    form.querySelectorAll('.field-error[data-json-error]').forEach(el => { el.hidden = true; });

    const submit = form.querySelector('button[type="submit"]');
    submit.disabled = true;
    saveStatus(form, 'Saving…', 'busy');
    let reread = true;

    try {
      const r = await api('/api/admin/settings', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      });
      const result = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);

      // The JSON configuration provider reloads asynchronously after the atomic
      // write. Give it one watcher tick before testing a newly saved connection, and before
      // asking which saved settings are still waiting on a restart.
      if (form.id === 'lidarr-connection-form' || needsRestart) {
        await new Promise(resolve => setTimeout(resolve, 600));
      }
      try {
        currentSettings = await (await api('/api/admin/settings', { cache: 'no-store' })).json();
      } catch {
        // Saved, but the page could not re-read the result; say which, rather than "Save failed".
        reread = false;
      }
      if (form.id === 'lidarr-connection-form') await loadLidarrOptions();
      renderOctoAddresses();
      renderRestartPending();
      renderSetupChecklist();
      // The scrobbling card reads the running settings, which catch up with the file a moment later.
      if (form.dataset.section?.startsWith('lastfm')) setTimeout(loadLastFmScrobbling, 700);
      form.querySelector('.form-actions')?.classList.remove('dirty');
      // A saved admin password is only ever shown as the placeholder; the typed value should
      // not stay in the page after it has been saved.
      form.querySelectorAll('input[type="password"][name]').forEach(input => {
        const [section, key] = input.name.split('.');
        const saved = currentSettings?.[section]?.[key];
        if (saved && saved === currentSettings?._meta?.SecretPlaceholder) input.value = saved;
      });
      markClean(form);
      const at = new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
      saveStatus(form, !reread ? 'Saved. Reload the page to see the current values.'
        : needsRestart ? `Saved at ${at}. Restart Octo to apply it.`
        : `Saved at ${at}`);
    } catch (err) {
      saveStatus(form, `Not saved: ${err.message}`, 'error');
    } finally {
      submit.disabled = false;
    }
  });
});

// Heart acquisition is a short priority chain, not a workflow graph. Keep all
// sources visible so disabling one never destroys the user's chosen order.
const heartSourceMeta = {
  Soulseek: { title: 'Soulseek', detail: 'Lossless FLAC from slskd peers', mark: 'b-slskd' },
  YouTube: { title: 'YouTube', detail: 'Lossy MP3 from the yt-dlp shim', mark: 'b-youtube' },
  Lidarr: { title: 'Lidarr', detail: 'Album automation through your Lidarr server', mark: 'b-lidarr' },
};
let heartSourceSteps = [];
let draggedHeartSourceRow = null;
let draggedHeartSourcePointer = null;
let heartSourceDragGhost = null;

function normalizeHeartSourceSteps(steps) {
  const seen = new Set();
  const normalized = [];
  (Array.isArray(steps) ? steps : []).forEach(step => {
    const source = step?.Source ?? step?.source;
    if (!heartSourceMeta[source] || seen.has(source)) return;
    seen.add(source);
    const legacyEnabled = step?.Enabled ?? step?.enabled ?? false;
    normalized.push({
      Source: source,
      SongEnabled: Boolean(step?.SongEnabled ?? step?.songEnabled ?? legacyEnabled),
      AlbumEnabled: Boolean(step?.AlbumEnabled ?? step?.albumEnabled ?? legacyEnabled),
    });
  });
  ['Soulseek', 'YouTube', 'Lidarr'].forEach(source => {
    if (!seen.has(source)) normalized.push({
      Source: source,
      SongEnabled: source === 'Soulseek',
      AlbumEnabled: source === 'Soulseek',
    });
  });
  return normalized;
}

function renderHeartSourceOrder(steps = heartSourceSteps) {
  const list = document.getElementById('heart-source-order');
  if (!list) return;
  heartSourceSteps = normalizeHeartSourceSteps(steps);
  list.innerHTML = heartSourceSteps.map((step, index) => {
    const meta = heartSourceMeta[step.Source];
    return `
      <div class="source-priority-row${step.SongEnabled || step.AlbumEnabled ? '' : ' is-disabled'}"
           data-source="${step.Source}" data-index="${index}">
        <button type="button" class="source-drag" draggable="true"
                aria-label="Drag ${meta.title} to reorder. Use arrow keys to move it."
                title="Drag to reorder; arrow keys also work">
          ${icon('i-dots-six-vertical')}
        </button>
        <span class="source-step" aria-hidden="true">${index + 1}</span>
        <span class="source-copy">
          <span class="source-title">${icon(meta.mark)}${meta.title}</span>
          <span class="source-detail">${meta.detail}</span>
        </span>
        <span class="source-heart-controls" role="group" aria-label="${meta.title} heart types">
          <span class="source-heart-choice">
            <span class="source-heart-label">Song hearts
              ${step.Source === 'Lidarr' ? `
                <span class="source-info" tabindex="0" role="img"
                      aria-label="A song heart asks Lidarr to acquire the entire album. Enable this if you want Lidarr to handle single-song requests anyway."
                      data-tooltip="A song heart asks Lidarr to acquire the entire album. Enable this if you want Lidarr to handle single-song requests anyway.">
                  ${icon('i-info')}
                </span>` : ''}
            </span>
            <label class="switch source-kind-switch">
              <input type="checkbox" data-heart-kind="SongEnabled" aria-label="Use ${meta.title} for song hearts" ${step.SongEnabled ? 'checked' : ''} />
              <span class="sw-track"></span><span class="sw-thumb"></span>
            </label>
          </span>
          <span class="source-heart-choice">
            <span class="source-heart-label">Album hearts</span>
            <label class="switch source-kind-switch">
              <input type="checkbox" data-heart-kind="AlbumEnabled" aria-label="Use ${meta.title} for album hearts" ${step.AlbumEnabled ? 'checked' : ''} />
              <span class="sw-track"></span><span class="sw-thumb"></span>
            </label>
          </span>
        </span>
      </div>`;
  }).join('');
  syncHeartSourceInput();
}

function updateStreamSettings() {
  const waitForLossless = document.getElementById('f-wait-for-lossless-on-play')?.checked;
  const activeSource = waitForLossless ? 'Lossless' : 'YouTube';
  document.querySelectorAll('[data-stream-source]').forEach(section => {
    section.hidden = section.dataset.streamSource !== activeSource;
  });
}

function syncPlaybackSourceControl() {
  const control = document.getElementById('f-playback-source');
  const wait = document.getElementById('f-wait-for-lossless-on-play');
  if (!control || !wait) return;
  control.value = wait.checked ? 'Lossless' : 'YouTube';
}

document.getElementById('f-playback-source')?.addEventListener('change', event => {
  const wait = document.getElementById('f-wait-for-lossless-on-play');
  if (!wait) return;
  wait.checked = event.target.value === 'Lossless';
  wait.dispatchEvent(new Event('input', { bubbles: true }));
  updateStreamSettings();
});

function updateRadioPublicationSettings() {
  const enabled = Boolean(document.getElementById('f-radio-streams')?.checked);
  const quality = document.getElementById('f-radio-stream-quality');
  const icyMetadata = document.getElementById('f-radio-icy-metadata');
  if (quality) quality.disabled = !enabled;
  if (icyMetadata) icyMetadata.disabled = !enabled;
  document.getElementById('radio-stream-quality-row')?.classList.toggle('is-disabled', !enabled);
  document.getElementById('radio-icy-metadata-row')?.classList.toggle('is-disabled', !enabled);
}

document.getElementById('f-radio-streams')?.addEventListener('change', updateRadioPublicationSettings);

// Delegated, so buttons rendered later (the setup checklist) work the same way.
document.addEventListener('click', event => {
  const opener = event.target.closest('[data-open-tab]');
  if (opener) openTab(opener.dataset.openTab);
});

function syncHeartSourceInput() {
  const input = document.getElementById('f-heart-download-sources');
  const help = document.getElementById('heart-source-order-help');
  if (input) {
    input.value = JSON.stringify(heartSourceSteps);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }
  if (help) {
    const noneEnabled = !heartSourceSteps.some(step => step.SongEnabled || step.AlbumEnabled);
    help.hidden = !noneEnabled;
    help.textContent = noneEnabled
      ? 'No heart download sources are enabled. Hearts will not acquire files.'
      : '';
  }
}

function moveHeartSource(from, to) {
  if (from === to || from < 0 || to < 0 ||
      from >= heartSourceSteps.length || to >= heartSourceSteps.length) return;
  const [step] = heartSourceSteps.splice(from, 1);
  heartSourceSteps.splice(to, 0, step);
  renderHeartSourceOrder();
  document.querySelector(`.source-priority-row[data-index="${to}"] .source-drag`)?.focus();
}

const heartSourceList = document.getElementById('heart-source-order');
heartSourceList?.addEventListener('change', event => {
  if (!event.target.matches('[data-heart-kind]')) return;
  const row = event.target.closest('.source-priority-row');
  heartSourceSteps[Number(row.dataset.index)][event.target.dataset.heartKind] = event.target.checked;
  const step = heartSourceSteps[Number(row.dataset.index)];
  row.classList.toggle('is-disabled', !step.SongEnabled && !step.AlbumEnabled);
  syncHeartSourceInput();
});
heartSourceList?.addEventListener('keydown', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle || !['ArrowUp', 'ArrowDown'].includes(event.key)) return;
  event.preventDefault();
  const from = Number(handle.closest('.source-priority-row').dataset.index);
  moveHeartSource(from, from + (event.key === 'ArrowUp' ? -1 : 1));
});
heartSourceList?.addEventListener('dragstart', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle) return;
  const row = handle.closest('.source-priority-row');
  event.dataTransfer.effectAllowed = 'move';
  event.dataTransfer.setData('text/plain', row.dataset.source);
  const bounds = row.getBoundingClientRect();
  heartSourceDragGhost = row.cloneNode(true);
  heartSourceDragGhost.classList.add('source-drag-ghost');
  heartSourceDragGhost.setAttribute('aria-hidden', 'true');
  heartSourceDragGhost.style.width = `${bounds.width}px`;
  document.body.appendChild(heartSourceDragGhost);
  event.dataTransfer.setDragImage(
    heartSourceDragGhost,
    Math.max(0, Math.min(bounds.width, event.clientX - bounds.left)),
    Math.max(0, Math.min(bounds.height, event.clientY - bounds.top)));
  draggedHeartSourceRow = row;
  row.classList.add('is-dragging');
});
heartSourceList?.addEventListener('dragend', event => {
  event.target.closest('.source-priority-row')?.classList.remove('is-dragging');
  syncHeartSourceOrderFromDom();
  draggedHeartSourceRow = null;
  heartSourceDragGhost?.remove();
  heartSourceDragGhost = null;
});
heartSourceList?.addEventListener('dragover', event => {
  const target = event.target.closest('.source-priority-row');
  if (!target || !draggedHeartSourceRow || target === draggedHeartSourceRow) return;
  event.preventDefault();
  previewHeartSourceRowMove(target, event.clientY);
});
heartSourceList?.addEventListener('drop', event => {
  event.preventDefault();
});
heartSourceList?.addEventListener('pointerdown', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle || event.pointerType === 'mouse') return;
  event.preventDefault();
  draggedHeartSourcePointer = event.pointerId;
  draggedHeartSourceRow = handle.closest('.source-priority-row');
  draggedHeartSourceRow.classList.add('is-dragging');
  handle.setPointerCapture(event.pointerId);
});
heartSourceList?.addEventListener('pointermove', event => {
  if (event.pointerId !== draggedHeartSourcePointer || !draggedHeartSourceRow) return;
  const target = document.elementFromPoint(event.clientX, event.clientY)
    ?.closest('.source-priority-row');
  if (target) previewHeartSourceRowMove(target, event.clientY);
});
heartSourceList?.addEventListener('pointerup', finishHeartSourcePointerDrag);
heartSourceList?.addEventListener('pointercancel', finishHeartSourcePointerDrag);

function finishHeartSourcePointerDrag(event) {
  if (event.pointerId !== draggedHeartSourcePointer) return;
  draggedHeartSourceRow?.classList.remove('is-dragging');
  syncHeartSourceOrderFromDom();
  draggedHeartSourceRow = null;
  draggedHeartSourcePointer = null;
}

function previewHeartSourceRowMove(target, clientY) {
  if (!heartSourceList || !draggedHeartSourceRow || target === draggedHeartSourceRow) return;
  const afterTarget = clientY > target.getBoundingClientRect().top + target.offsetHeight / 2;
  heartSourceList.insertBefore(draggedHeartSourceRow, afterTarget ? target.nextSibling : target);
  refreshHeartSourceRowNumbers();
}

function refreshHeartSourceRowNumbers() {
  heartSourceList?.querySelectorAll('.source-priority-row').forEach((row, index) => {
    row.dataset.index = index;
    const number = row.querySelector('.source-step');
    if (number) number.textContent = String(index + 1);
  });
}

function syncHeartSourceOrderFromDom() {
  if (!heartSourceList) return;
  const bySource = new Map(heartSourceSteps.map(step => [step.Source, step]));
  heartSourceSteps = [...heartSourceList.querySelectorAll('.source-priority-row')]
    .map(row => bySource.get(row.dataset.source))
    .filter(Boolean);
  refreshHeartSourceRowNumbers();
  syncHeartSourceInput();
}

// The count comes from _meta rather than a second fetch, so the button says how much it
// would actually forget. A button labelled "Forget rejected peers" on an empty list looks
// broken when clicking it changes nothing.
function renderRejectedPeerCount() {
  const button = document.getElementById('rejected-peers-clear');
  if (!button) return;
  const count = currentSettings?._meta?.RejectedPeerCount ?? 0;
  button.textContent = count > 0 ? `Forget ${count} rejected peer${count === 1 ? '' : 's'}` : 'Nothing rejected yet';
  button.disabled = count === 0;
}

document.getElementById('rejected-peers-clear')?.addEventListener('click', async event => {
  const button = event.currentTarget;
  const count = currentSettings?._meta?.RejectedPeerCount ?? 0;
  if (!count) return;
  if (!(await askConfirm(`Forget ${count} rejected peer${count === 1 ? '' : 's'}?`, 'Those files become downloadable again.', 'Forget'))) return;
  try {
    const response = await api('/api/admin/soulseek/rejected-peers/clear', { method: 'POST' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();
    // Only the count changed. Reloading every form here threw away unsaved edits elsewhere.
    currentSettings = await (await api('/api/admin/settings', { cache: 'no-store' })).json();
    renderRejectedPeerCount();
    note(button, `Forgot ${body.cleared} rejected peer${body.cleared === 1 ? '' : 's'}.`);
  } catch (error) {
    note(button, `Could not clear: ${error.message}`, 'error');
  }
});

// ---- Genre mapping rules -------------------------------------------------
//
// An ORDERED list, not the pinned-stations editor: "applied in order, first match wins" is
// the whole semantics of the table, and an unordered editor cannot express it. Drag plus
// arrow keys, so reordering is never mouse-only.

let genreRules = [];

// Spread ...rule so a field a newer Octo adds is not erased when an older browser session
// edits a row, the same guard normalizeRadioStation makes.
function normalizeGenreRule(rule = {}) {
  return {
    ...rule,
    Id: rule.Id ?? rule.id ?? '',
    Pattern: rule.Pattern ?? rule.pattern ?? '',
    Genre: rule.Genre ?? rule.genre ?? '',
    Match: rule.Match ?? rule.match ?? 'Contains',
    Enabled: (rule.Enabled ?? rule.enabled ?? true) !== false,
  };
}

function escapeGenreAttr(value) {
  return String(value ?? '').replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}

function renderGenreMappings(rules = genreRules) {
  const list = document.getElementById('genre-mapping-list');
  if (!list) return;
  genreRules = (Array.isArray(rules) ? rules : []).map(normalizeGenreRule);
  list.innerHTML = genreRules.map((rule, index) => `
      <div class="source-priority-row genre-mapping-row${rule.Enabled ? '' : ' is-disabled'}" data-index="${index}">
        <button type="button" class="source-drag" draggable="true"
                aria-label="Drag rule ${index + 1} to reorder. Use arrow keys to move it."
                title="Drag to reorder; arrow keys also work">
          ${icon('i-dots-six-vertical')}
        </button>
        <span class="source-step" aria-hidden="true">${index + 1}</span>
        <input class="set-input" data-genre-field="Pattern" value="${escapeGenreAttr(rule.Pattern)}"
               placeholder="pattern" aria-label="Rule ${index + 1} pattern" />
        <input class="set-input" data-genre-field="Genre" value="${escapeGenreAttr(rule.Genre)}"
               placeholder="genre (empty drops it)" aria-label="Rule ${index + 1} genre" />
        <select class="set-input" data-genre-field="Match" aria-label="Rule ${index + 1} match mode">
          <option value="Contains"${rule.Match === 'Contains' ? ' selected' : ''}>contains</option>
          <option value="Exact"${rule.Match === 'Exact' ? ' selected' : ''}>exact</option>
        </select>
        <label class="switch source-kind-switch">
          <input type="checkbox" data-genre-field="Enabled" aria-label="Enable rule ${index + 1}" ${rule.Enabled ? 'checked' : ''} />
          <span class="sw-track"></span><span class="sw-thumb"></span>
        </label>
        <button class="btn btn-ghost btn-icon" type="button" data-genre-remove aria-label="Remove rule ${index + 1}" title="Remove this rule">${icon('i-trash')}</button>
      </div>`).join('');
  syncGenreMappingsInput(false);
}

function readGenreMappingRows() {
  document.querySelectorAll('#genre-mapping-list .source-priority-row').forEach(row => {
    const rule = genreRules[Number(row.dataset.index)];
    if (!rule) return;
    row.querySelectorAll('[data-genre-field]').forEach(input => {
      const field = input.dataset.genreField;
      rule[field] = field === 'Enabled' ? input.checked : input.value;
    });
  });
}

function syncGenreMappingsInput(showErrors = true) {
  readGenreMappingRows();
  const input = document.getElementById('genre-mappings-json');
  const error = document.getElementById('genre-mapping-error');
  if (!input) return true;

  let message = '';
  const seen = new Set();
  for (const rule of genreRules) {
    const pattern = (rule.Pattern || '').trim().toLowerCase();
    if (!pattern) { message = 'Every rule needs a pattern.'; break; }
    if (pattern.length > 60) { message = `"${pattern}" is longer than 60 characters.`; break; }
    // A later duplicate could never fire, so it is a mistake rather than a preference.
    if (seen.has(pattern)) { message = `"${pattern}" appears twice; only the first would ever match.`; break; }
    seen.add(pattern);
    if ((rule.Genre || '').trim().length > 60) { message = `The genre for "${pattern}" is longer than 60 characters.`; break; }
  }
  if (genreRules.length > 200) message = 'At most 200 rules.';

  if (error) {
    error.textContent = message;
    error.hidden = !message || !showErrors;
  }
  if (message) return false;

  input.value = JSON.stringify(genreRules.map(rule => ({
    ...rule,
    Pattern: (rule.Pattern || '').trim(),
    Genre: (rule.Genre || '').trim(),
  })));
  return true;
}

function moveGenreRule(from, to) {
  if (from === to || from < 0 || to < 0 || from >= genreRules.length || to >= genreRules.length) return;
  const [rule] = genreRules.splice(from, 1);
  genreRules.splice(to, 0, rule);
  renderGenreMappings();
  document.querySelector(`#genre-mapping-list .source-priority-row[data-index="${to}"] .source-drag`)?.focus();
}

function syncGenreUnknownRow() {
  const row = document.getElementById('genre-unknown-row');
  if (row) row.hidden = document.getElementById('f-genre-on-empty')?.value !== 'Unknown';
}

function renderGenreBlocklist(values) {
  const visible = document.getElementById('f-genre-blocklist');
  if (visible) visible.value = (Array.isArray(values) ? values : []).join(', ');
  syncGenreBlocklistInput();
}

function syncGenreBlocklistInput() {
  const visible = document.getElementById('f-genre-blocklist');
  const hidden = document.getElementById('genre-blocklist-json');
  if (!visible || !hidden) return;
  const entries = visible.value.split(',')
    .map(entry => entry.trim().replace(/\s+/g, ' ').toLowerCase())
    .filter(Boolean);
  hidden.value = JSON.stringify([...new Set(entries)]);
}

const genreMappingList = document.getElementById('genre-mapping-list');
genreMappingList?.addEventListener('input', () => syncGenreMappingsInput());
genreMappingList?.addEventListener('change', event => {
  if (event.target.matches('[data-genre-field="Enabled"]')) {
    readGenreMappingRows();
    renderGenreMappings();
    return;
  }
  syncGenreMappingsInput();
});
genreMappingList?.addEventListener('click', event => {
  if (!event.target.closest('[data-genre-remove]')) return;
  const row = event.target.closest('.source-priority-row');
  readGenreMappingRows();
  genreRules.splice(Number(row.dataset.index), 1);
  renderGenreMappings();
});
genreMappingList?.addEventListener('keydown', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle || !['ArrowUp', 'ArrowDown'].includes(event.key)) return;
  event.preventDefault();
  readGenreMappingRows();
  const from = Number(handle.closest('.source-priority-row').dataset.index);
  moveGenreRule(from, from + (event.key === 'ArrowUp' ? -1 : 1));
});

let draggedGenreRuleIndex = null;
genreMappingList?.addEventListener('dragstart', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle) return;
  const row = handle.closest('.source-priority-row');
  readGenreMappingRows();
  draggedGenreRuleIndex = Number(row.dataset.index);
  event.dataTransfer.effectAllowed = 'move';
  event.dataTransfer.setData('text/plain', String(draggedGenreRuleIndex));
  row.classList.add('is-dragging');
});
genreMappingList?.addEventListener('dragover', event => {
  if (draggedGenreRuleIndex === null) return;
  if (event.target.closest('.source-priority-row')) event.preventDefault();
});
genreMappingList?.addEventListener('drop', event => {
  const target = event.target.closest('.source-priority-row');
  if (draggedGenreRuleIndex === null || !target) return;
  event.preventDefault();
  moveGenreRule(draggedGenreRuleIndex, Number(target.dataset.index));
  draggedGenreRuleIndex = null;
});
genreMappingList?.addEventListener('dragend', event => {
  event.target.closest('.source-priority-row')?.classList.remove('is-dragging');
  draggedGenreRuleIndex = null;
});

// Fetched, never duplicated here: a second copy of the preset in JS is a table the dashboard
// and the tests could disagree about.
document.getElementById('genre-preset-broad')?.addEventListener('click', async () => {
  readGenreMappingRows();
  const existing = genreRules.filter(rule => rule.Pattern || rule.Genre).length;
  if (existing > 0 && !(await askConfirm(`Replace your ${existing} rule${existing === 1 ? '' : 's'}?`, 'They are swapped for the broad preset. Nothing is saved until you press Save.', 'Replace rules'))) return;
  try {
    const response = await api('/api/admin/genre/presets');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();
    renderGenreMappings(body.broad);
    note(document.querySelector('#genre-form .genre-preset-actions'), `Loaded ${body.broad.length} rules. Save to apply.`, 'info');
  } catch (error) {
    note(document.querySelector('#genre-form .genre-preset-actions'), `Could not load the preset: ${error.message}`, 'error');
  }
});
document.getElementById('genre-add-custom')?.addEventListener('click', () => {
  readGenreMappingRows();
  genreRules.push(normalizeGenreRule({ Pattern: '', Genre: '' }));
  renderGenreMappings();
});
document.getElementById('f-genre-on-empty')?.addEventListener('change', syncGenreUnknownRow);
document.getElementById('f-genre-blocklist')?.addEventListener('input', syncGenreBlocklistInput);

// ---- Genre backfill ------------------------------------------------------
//
// Every call here is gated on a verified Navidrome admin session, because /api/admin has no
// authentication of its own and this one rewrites tags. A 401 prompts for credentials once
// and retries, reusing the browse sign-in the directory picker already uses.

let genreBackfillPoll = null;
let lastBackfillRun = null;
let backfillScopeInitialised = false;

const backfillScopeLabels = { OctoDownloads: 'downloads Octo made', WholeLibrary: 'the whole library' };

async function genreBackfillFetch(url, options = {}, retry = true) {
  const response = await api(url, options);
  if (response.status === 401 && retry) {
    const holder = document.getElementById('genre-backfill-status');
    if (await browseAuthenticate(holder ?? document.createElement('div'))) {
      return genreBackfillFetch(url, options, false);
    }
  }
  return response;
}

function renderGenreBackfill(run) {
  const status = document.getElementById('genre-backfill-status');
  const results = document.getElementById('genre-backfill-results');
  if (!status || !results) return;

  const running = run.status === 'Running';
  const previewed = run.status === 'Completed' && run.dryRun;
  const scopeSelect = document.getElementById('genre-backfill-scope');
  // Apply writes whatever the CURRENT scope and rules produce, so it is only offered while both
  // still match what this preview was made from.
  const scopeChanged = previewed && scopeSelect && scopeSelect.value !== run.scope;
  const rulesChanged = previewed && run.settingsChanged === true;
  const hint = document.getElementById('genre-backfill-scope-hint');
  if (hint) {
    hint.hidden = !(scopeChanged || rulesChanged);
    hint.textContent = rulesChanged
      ? 'Your genre rules changed after this preview. Preview again before applying.'
      : 'You changed which files. Preview again before applying.';
  }

  document.getElementById('genre-backfill-cancel').hidden = !running;
  document.getElementById('genre-backfill-apply').hidden = !previewed || run.changed === 0 || scopeChanged || rulesChanged;
  document.getElementById('genre-backfill-resume').hidden = !run.canResume || run.settingsChanged === true;
  document.getElementById('genre-backfill-undo').hidden = !run.canUndo || running;
  document.getElementById('genre-backfill-preview').disabled = running;

  if (run.status === 'Idle') {
    status.innerHTML = '';
    results.innerHTML = '';
    return;
  }

  const label = {
    Running: run.dryRun ? 'Previewing' : 'Applying',
    Completed: run.dryRun ? 'Preview finished' : 'Finished',
    Cancelled: 'Cancelled',
    Interrupted: 'Interrupted',
    Failed: 'Stopped',
  }[run.status] ?? run.status;

  const counts = [
    `${run.processed} of ${run.total} files`,
    `${run.changed} would change`,
    run.cleared ? `${run.cleared} cleared` : null,
    run.skipped ? `${run.skipped} skipped` : null,
    run.failed ? `${run.failed} failed` : null,
  ].filter(Boolean).join(' · ');

  status.innerHTML = `
    <div class="set-info">
      <div class="set-info-t">${esc(label)}${run.dryRun ? '' : ' (writing)'}</div>
      <div class="set-info-d">${esc(counts)}${run.reason ? `. ${esc(run.reason)}` : ''}</div>
    </div>`;

  if (!run.preview?.length) {
    results.innerHTML = run.errors?.length
      ? `<div class="field-error" role="alert">${esc(run.errors[run.errors.length - 1])}</div>`
      : '';
    return;
  }

  // Reuses the config-sources table shell, which is a grid rather than a real <table>.
  const rows = run.preview.slice(0, 200).map(change => `
    <div class="config-row genre-change-row">
      <span class="key">${esc(change.path)}</span>
      <span class="value">${esc((change.before || []).join(', ')) || '<em>none</em>'}</span>
      <span class="value${change.action === 'Clear' ? ' empty' : ''}">${change.action === 'Clear' ? 'cleared' : esc((change.after || []).join(', '))}</span>
      <span class="value">${esc(change.rule || '')}</span>
    </div>`).join('');

  results.innerHTML = `
    <div class="config-table">
      <div class="config-row config-row-head genre-change-row">
        <span>File</span><span>Before</span><span>After</span><span>Rule</span>
      </div>
      ${rows}
    </div>
    ${run.preview.length > 200 ? `<p class="set-info-d">Showing the first 200 of ${run.preview.length} changes.</p>` : ''}`;
}

async function loadGenreBackfill(retry = false) {
  const response = await genreBackfillFetch('/api/admin/genre/backfill', {}, retry);
  if (!response.ok) {
    // Signed out or Octo away: stop asking every two seconds; the page catches up when opened.
    if (genreBackfillPoll) { clearInterval(genreBackfillPoll); genreBackfillPoll = null; }
    return null;
  }
  const run = await response.json();
  // Accepted but still reading the library: show it as starting, or the page reads the last
  // run's answer right after Start and never begins watching.
  if (run.pending && run.status !== 'Running')
    Object.assign(run, { status: 'Running', starting: true, processed: 0, total: 0, changed: 0, preview: [] });
  const previous = lastBackfillRun;
  lastBackfillRun = run;

  // Open on the scope of a finished preview, so the Apply it offers is the one it describes.
  const scopeSelect = document.getElementById('genre-backfill-scope');
  if (!backfillScopeInitialised && scopeSelect && run.status === 'Completed' && run.dryRun && run.scope) {
    scopeSelect.value = run.scope;
  }
  backfillScopeInitialised = true;
  renderGenreBackfill(run);

  // The status line is not a live region (it rewrites every two seconds while running), so the
  // end of a run is announced once, here.
  if (previous?.status === 'Running' && run.status !== 'Running') {
    backfillNote(`${run.dryRun ? 'Preview' : 'Genre run'} ${run.status === 'Completed' ? 'finished' : run.status.toLowerCase()}: ${run.changed} file(s)${run.dryRun ? ' would change' : ' changed'}.`,
      run.status === 'Failed' ? 'error' : 'ok');
  }

  // Poll only while something is happening, so an idle dashboard is not making a request a
  // second forever.
  if (run.status === 'Running') {
    if (!genreBackfillPoll) genreBackfillPoll = setInterval(() => loadGenreBackfill(), 2000);
  } else if (genreBackfillPoll) {
    clearInterval(genreBackfillPoll);
    genreBackfillPoll = null;
  }
  return run;
}

function backfillNote(message, kind = 'ok') {
  note(document.querySelector('#genre-backfill-actions .genre-preset-actions'), message, kind);
}

async function startGenreBackfill(dryRun, scopeOverride = null) {
  const scope = scopeOverride ?? document.getElementById('genre-backfill-scope')?.value ?? 'OctoDownloads';

  let confirmPath = null;
  if (scope === 'WholeLibrary' && !dryRun) {
    const current = await loadGenreBackfill();
    const expected = current?.musicPath ?? '';
    confirmPath = await askDialog({
      title: 'Write genres to your whole library?',
      message: `This rewrites tags on every audio file under:\n${expected}\nincluding music Octo never downloaded. Type that path exactly to continue.`,
      confirm: 'Write genres', danger: true,
      input: { label: 'Music folder', placeholder: expected, mustEqual: expected },
    });
    if (confirmPath === null) return;
  }

  const response = await genreBackfillFetch('/api/admin/genre/backfill', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ scope, dryRun, confirm: confirmPath }),
  });

  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    backfillNote(body.error || `Could not start: HTTP ${response.status}`, 'error');
    return;
  }
  backfillNote(dryRun ? 'Previewing. Nothing is being written.' : 'Applying changes.', 'info');
  await loadGenreBackfill();
}

document.getElementById('genre-backfill-preview')?.addEventListener('click', () => startGenreBackfill(true));
document.getElementById('genre-backfill-scope')?.addEventListener('change', () => {
  if (lastBackfillRun) renderGenreBackfill(lastBackfillRun);
});
document.getElementById('genre-backfill-apply')?.addEventListener('click', async () => {
  const run = await loadGenreBackfill();
  if (!run || run.settingsChanged) return;
  const scopeLabel = backfillScopeLabels[run.scope] ?? run.scope;
  if (!(await askConfirm(`Write new genres to ${run.changed} file${run.changed === 1 ? '' : 's'}?`, `In ${scopeLabel}. Only the genre is recorded for undo; anything else the tag library cannot round-trip is lost.`, 'Write genres', true))) return;
  // The previewed scope, not whatever the dropdown says by now.
  await startGenreBackfill(false, run.scope);
});
document.getElementById('genre-backfill-cancel')?.addEventListener('click', async () => {
  const response = await genreBackfillFetch('/api/admin/genre/backfill/cancel', { method: 'POST' });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    backfillNote(body.error || `Could not cancel: HTTP ${response.status}`, 'error');
    return;
  }
  backfillNote('Cancelling after the current file.', 'info');
  await loadGenreBackfill();
});
document.getElementById('genre-backfill-resume')?.addEventListener('click', async () => {
  const run = lastBackfillRun;
  // Resuming a preview writes nothing; resuming an apply writes tags, so it asks again.
  if (run && !run.dryRun
      && !(await askConfirm('Resume writing genres?', `From file ${run.processed + 1} of ${run.total}.`, 'Resume'))) return;
  const response = await genreBackfillFetch('/api/admin/genre/backfill/resume', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { backfillNote(body.error || 'Could not resume.', 'error'); return; }
  await loadGenreBackfill();
});
document.getElementById('genre-backfill-undo')?.addEventListener('click', async () => {
  if (!(await askConfirm('Put back the genres?', 'Every file changed since the last undo gets its genre back. Only the genre is restored, and files moved since then stay changed.', 'Put back genres'))) return;
  const response = await genreBackfillFetch('/api/admin/genre/backfill/undo', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { backfillNote(body.error || 'Could not undo.', 'error'); return; }
  backfillNote('Restoring genres.', 'info');
  await loadGenreBackfill();
});

// ---- Mark explicit songs ----------------------------------------------------------
// Look up (writes nothing), then mark what the lookup found explicit or clean, then undo.
// Every call needs the browse sign-in, like the genre re-tag.

let explicitPoll = null;
let lastExplicitRun = null;
let explicitShow = '';

const explicitWords = { explicit: 'Explicit', clean: 'Clean edit', notExplicit: 'Not explicit', unsure: 'Unsure' };

function explicitNote(message, kind = 'ok') {
  note(document.querySelector('#explicit-backfill-actions .genre-preset-actions'), message, kind);
}

function renderExplicitBackfill(run) {
  const status = document.getElementById('explicit-backfill-status');
  const results = document.getElementById('explicit-backfill-results');
  const filter = document.getElementById('explicit-backfill-filter');
  if (!status || !results || !filter) return;
  const running = run.status === 'Running';
  document.getElementById('explicit-backfill-cancel').hidden = !running;
  document.getElementById('explicit-backfill-apply').hidden = running || !run.canApply;
  document.getElementById('explicit-backfill-apply').textContent = run.toWrite ? `Mark ${run.toWrite.toLocaleString()} song${run.toWrite === 1 ? '' : 's'}` : 'Mark songs';
  document.getElementById('explicit-backfill-resume').hidden = running || !run.canResume;
  document.getElementById('explicit-backfill-undo').hidden = running || !run.canUndo;
  document.getElementById('explicit-backfill-preview').disabled = running;

  if (run.status === 'Idle') {
    status.innerHTML = '';
    results.innerHTML = '';
    filter.hidden = true;
    return;
  }

  const writing = run.mode === 'Apply';
  const undoing = run.mode === 'Undo';
  const label = running
    ? (writing ? 'Marking songs' : undoing ? 'Taking the marks out' : 'Looking songs up')
    : ({ Completed: 'Done', Cancelled: 'Stopped', Interrupted: 'Interrupted', Failed: 'Stopped' }[run.status] ?? run.status);
  const n = v => (v ?? 0).toLocaleString();
  const counts = writing || undoing
    ? [`${n(run.stepDone)} of ${n(run.stepTotal)} songs`, `${n(run.written)} ${undoing ? 'put back' : 'marked'}`,
       run.leftAlone ? `${n(run.leftAlone)} changed since, left alone` : null, run.failed ? `${n(run.failed)} failed` : null]
    : [`${n(run.processed)} of ${n(run.total)} songs looked up`, `${n(run.explicit)} explicit`, `${n(run.clean)} clean`,
       `${n(run.notExplicit)} not explicit`, `${n(run.unsure)} unsure`,
       run.alreadyMarked ? `${n(run.alreadyMarked)} already marked` : null, run.failed ? `${n(run.failed)} unreadable` : null];
  status.innerHTML = `
    <div class="set-info">
      <div class="set-info-t">${esc(label)}</div>
      <div class="set-info-d">${esc(counts.filter(Boolean).join(' · '))}${run.reason ? `. ${esc(run.reason)}` : ''}</div>
      ${running && run.lastSong ? `<div class="set-info-more">${esc(run.lastSong)}</div>` : ''}
    </div>`;

  const kinds = ['explicit', 'clean', 'unsure', 'notExplicit'].filter(kind => (run[kind] ?? 0) > 0);
  filter.hidden = kinds.length === 0;
  filter.innerHTML = [['', 'All'], ...kinds.map(kind => [kind, explicitWords[kind]])].map(([kind, word]) =>
    `<button class="btn btn-ghost btn-sm" type="button" data-show="${kind}" aria-pressed="${explicitShow === kind}">${esc(word)}</button>`).join('');

  if (!run.rows?.length) {
    results.innerHTML = run.errors?.length ? `<div class="field-error" role="alert">${esc(run.errors[run.errors.length - 1])}</div>` : '';
    return;
  }
  const rows = run.rows.map(row => `
    <div class="config-row explicit-row">
      <span class="key">${esc(row.artist && row.title ? `${row.title} by ${row.artist}` : row.path)}</span>
      <span class="value${row.outcome === 'explicit' || row.outcome === 'clean' ? ' found-explicit' : ''}">${esc(explicitWords[row.outcome] ?? row.outcome)}${row.written ? ', marked' : ''}</span>
      <span class="value">${esc(row.how)}</span>
    </div>`).join('');
  const shown = explicitShow ? (run[explicitShow] ?? 0) : (run.explicit + run.clean + run.notExplicit + run.unsure);
  results.innerHTML = `
    <div class="config-table">
      <div class="config-row config-row-head explicit-row"><span>Song</span><span>Found</span><span>How</span></div>
      ${rows}
    </div>
    ${shown > run.rows.length ? `<p class="set-info-d">Showing ${run.rows.length.toLocaleString()} of ${shown.toLocaleString()}.</p>` : ''}`;
}

async function loadExplicitBackfill(retry = false) {
  const query = explicitShow ? `?show=${encodeURIComponent(explicitShow)}` : '';
  const response = await genreBackfillFetch(`/api/admin/explicit/backfill${query}`, {}, retry);
  if (!response.ok) return null;
  const run = await response.json();
  const previous = lastExplicitRun;
  lastExplicitRun = run;
  renderExplicitBackfill(run);
  if (previous?.status === 'Running' && run.status !== 'Running') {
    explicitNote(run.reason || 'Finished.', run.status === 'Failed' ? 'error' : 'ok');
  }
  if (run.status === 'Running') {
    if (!explicitPoll) explicitPoll = setInterval(() => loadExplicitBackfill(), 2000);
  } else if (explicitPoll) {
    clearInterval(explicitPoll);
    explicitPoll = null;
  }
  return run;
}

async function explicitPost(path, body = null) {
  const response = await genreBackfillFetch(`/api/admin/explicit/backfill/${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : null,
  });
  const answer = await response.json().catch(() => ({}));
  if (!response.ok) explicitNote(answer.error || `Could not do that: HTTP ${response.status}`, 'error');
  return response.ok;
}

// A short run can be over before the first poll, so the note says how it ended, not that it began.
async function explicitStarted(words) {
  await new Promise(resolve => setTimeout(resolve, 600));
  const run = await loadExplicitBackfill();
  if (run && run.status !== 'Running') explicitNote(run.reason || 'Done.', run.status === 'Failed' ? 'error' : 'ok');
  else explicitNote(words, 'info');
}

document.getElementById('explicit-backfill-filter')?.addEventListener('click', event => {
  const button = event.target.closest('button[data-show]');
  if (!button) return;
  explicitShow = button.dataset.show;
  loadExplicitBackfill();
});
document.getElementById('explicit-backfill-preview')?.addEventListener('click', async () => {
  if (await explicitPost('preview')) {
    explicitNote('Looking songs up. Nothing is being written.', 'info');
    await loadExplicitBackfill();
  }
});
document.getElementById('explicit-backfill-resume')?.addEventListener('click', async () => {
  if (await explicitPost('resume')) await loadExplicitBackfill();
});
document.getElementById('explicit-backfill-cancel')?.addEventListener('click', async () => {
  if (await explicitPost('cancel')) {
    explicitNote('Stopping after the current song.', 'info');
    await loadExplicitBackfill();
  }
});
document.getElementById('explicit-backfill-apply')?.addEventListener('click', async () => {
  const run = await loadExplicitBackfill(true);
  if (!run?.canApply) return;
  const expected = run.musicPath ?? '';
  const confirm = await askDialog({
    title: `Mark ${run.toWrite.toLocaleString()} song${run.toWrite === 1 ? '' : 's'} explicit or clean?`,
    message: `This writes the mark into the files under:\n${expected}\nincluding music Octo never downloaded. Type that path exactly to continue.`,
    confirm: 'Mark songs', danger: true,
    input: { label: 'Music folder', placeholder: expected, mustEqual: expected },
  });
  if (confirm === null) return;
  if (await explicitPost('apply', { confirm })) await explicitStarted('Marking songs.');
});
document.getElementById('explicit-backfill-undo')?.addEventListener('click', async () => {
  if (!(await askConfirm('Take the marks back out?', 'Every song the last run marked loses its mark, except a file changed since.', 'Take them out'))) return;
  if (await explicitPost('undo')) await explicitStarted('Taking the marks out.');
});

// ---- Cover art: the soft covers wall ------------------------------------------
// Scan (reads the songs only), pick on the wall, find better covers (looks the picked albums
// up, writes nothing), replace. One button at the bottom always says the next step. Undo puts
// every replaced cover back.

let coverPoll = null;
let lastCoverRun = null;
let coverControlsSet = false;
let coverPicked = new Set();
let coverListRunId = null;
let coverShowAll = false;
let coverPace = null;
const COVER_TILE_CAP = 600;

const coverEl = id => document.getElementById(id);

function coverNote(message, kind = 'ok') {
  note(coverEl('cover-head')?.closest('.cover-summary'), message, kind);
}

function coverRows(run) { return run?.preview || []; }

// Which rows can be picked right now: every soft album after a scan, only the ones with a
// larger cover after a preview, none while running or after a replace.
function coverPickable(run, row) {
  if (!run || run.status === 'Running' || run.undo) return false;
  if (run.mode === 'Scan') return true;
  if (run.mode === 'Preview') return row.result === 'found';
  return false;
}

function coverTile(run, row) {
  const pickable = coverPickable(run, row);
  const picked = pickable && coverPicked.has(row.id);
  const now = `/api/admin/covers/upgrade/thumb/${encodeURIComponent(row.id)}`;
  const found = `${now}?found=true`;
  // A preview shows the cover it found over the one the album has; once replaced, the album's
  // own cover is the new one, so it is shown alone.
  const showFound = row.result === 'found';
  const px = side => (side > 0 ? `${side} px` : 'No cover');
  let badge;
  if (row.result === 'soft') badge = `<span class="cover-badge">${esc(px(row.fromSide))}</span>`;
  else if (row.result === 'found' && row.looksSame === false) badge = `<span class="cover-badge warn" title="This cover does not look like the one the album has now. It may be another edition, or another album with the same name.">Different art · ${esc(px(row.toSide))}</span>`;
  else if (row.result === 'found') badge = `<span class="cover-badge">${esc(px(row.fromSide))} → ${esc(px(row.toSide))}</span>`;
  else if (row.result === 'upgraded') badge = `<span class="cover-badge done">${icon('i-check')} ${esc(px(row.toSide))}</span>`;
  else badge = '<span class="cover-badge">Nothing larger</span>';
  const art = showFound
    ? `<img loading="lazy" alt="" src="${found}" onerror="this.src='${now}';this.onerror=null"><img class="cover-was" loading="lazy" alt="" src="${now}" onerror="this.remove()">`
    : row.result === 'upgraded'
      ? `<img loading="lazy" alt="" src="${now}" onerror="this.src='${found}';this.onerror=null">`
      : `<img loading="lazy" alt="" src="${now}" onerror="this.remove()">`;
  const name = `${row.album || '(no album)'} by ${row.artist}`;
  const sub = [row.artist, row.result === 'found' || row.result === 'upgraded' ? row.source : null].filter(Boolean).join(' · ');
  return `<button type="button" class="cover-tile" role="${pickable ? 'checkbox' : 'listitem'}" data-cover-id="${esc(row.id)}"
      ${pickable ? `aria-checked="${picked}"` : ''} ${!pickable && (row.result === 'none') ? 'aria-disabled="true"' : ''}
      aria-label="${esc(name)}" title="${esc(row.folder)}">
    <span class="cover-art">${art}${pickable ? `<span class="cover-check">${icon('i-check')}</span>` : ''}${badge}</span>
    <span class="cover-title">${esc(row.album || '(no album)')}</span>
    <span class="cover-artist">${esc(sub)}</span>
  </button>`;
}

function renderCoverBar(run) {
  const rows = coverRows(run);
  const running = run.status === 'Running';
  const pickableRows = rows.filter(row => coverPickable(run, row));
  const count = pickableRows.filter(row => coverPicked.has(row.id)).length;
  const go = coverEl('cover-go');
  const hint = coverEl('cover-hint');
  const countEl = coverEl('cover-count');

  coverEl('cover-stop').hidden = !running;
  // A scan is quick to start over, and starting over always uses the newest way of reading;
  // only lookups and replaces, which take minutes, are worth resuming.
  coverEl('cover-resume').hidden = running || !run.canResume || run.mode === 'Scan';
  coverEl('cover-undo').hidden = running || !run.canUndo;
  coverEl('cover-select').hidden = pickableRows.length === 0;
  go.hidden = true;
  countEl.textContent = '';
  hint.textContent = '';

  if (running) {
    hint.textContent = run.undo ? 'Putting the old covers back.'
      : run.mode === 'Scan' ? 'Reading your songs. Nothing is looked up or changed.'
      : run.mode === 'Preview' ? 'Looking up each picked album, about 3 seconds apiece. Nothing changes yet.'
      : 'Replacing covers. Every old cover is kept, so you can undo.';
  } else if (run.undo) {
    hint.textContent = run.status === 'Completed' ? 'Navidrome is picking the old covers back up.' : (run.reason || '');
  } else if (run.mode === 'Scan' && pickableRows.length) {
    countEl.textContent = `${count} selected`;
    hint.textContent = 'Looks up a larger cover for each. Nothing changes yet.';
    go.textContent = 'Find better covers';
    go.hidden = false;
  } else if (run.mode === 'Preview' && pickableRows.length) {
    countEl.textContent = `${count.toLocaleString()} selected`;
    hint.textContent = 'Your old covers are kept, so you can undo.';
    go.textContent = `Replace ${count} cover${count === 1 ? '' : 's'}`;
    go.hidden = false;
  } else if (run.mode === 'Apply' && run.status === 'Completed') {
    hint.textContent = run.files ? 'Navidrome is picking them up.' : 'Nothing needed replacing.';
  } else if (run.status === 'Cancelled' || run.status === 'Interrupted') {
    hint.textContent = run.mode === 'Scan' ? 'Stopped part way. Scan again to see every album.' : (run.reason || 'Stopped.');
  }
  coverEl('cover-bar').hidden = run.status === 'Idle'
    || (go.hidden && coverEl('cover-stop').hidden && coverEl('cover-resume').hidden && coverEl('cover-undo').hidden && !hint.textContent);
}

function renderCovers(run) {
  if (!coverEl('cover-wall')) return;
  const rows = coverRows(run);
  const running = run.status === 'Running';

  // A fresh list from a finished run: pick everything worth carrying to the next step.
  if (!running && run.runId && run.runId !== coverListRunId) {
    coverListRunId = run.runId;
    coverShowAll = false;
    // A cover that does not look like the album's own is shown, not pre-picked: it may be
    // another edition, or another album that shares the name.
    coverPicked = new Set(rows.filter(row => coverPickable(run, row) && row.looksSame !== false).map(row => row.id));
  }

  const progress = coverEl('cover-progress');
  progress.hidden = !running;
  const byAlbum = run.albumsTotal > 0;
  const done = byAlbum ? run.albumsDone : run.songsTotal ? run.songsRead : run.processed;
  const all = byAlbum ? run.albumsTotal : run.songsTotal || run.total;
  const starting = running && (run.starting || !run.total);
  progress.classList.toggle('starting', starting);
  if (running && !starting) coverEl('cover-progress-fill').style.width = `${all ? Math.min(100, Math.round(100 * done / all)) : 0}%`;

  const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
  let head = '';
  let sub = '';
  if (run.status === 'Idle') head = '';
  else if (run.undo) {
    head = running ? 'Putting covers back' : run.status === 'Completed' ? 'Covers put back' : 'Undo stopped';
    sub = `${plural(run.files, 'song')}`;
  } else if (running && (run.starting || !run.total)) {
    head = 'Getting started';
    sub = 'Listing your songs';
  } else if (running) {
    head = { Scan: 'Scanning', Preview: 'Finding better covers', Apply: 'Replacing covers' }[run.mode] ?? 'Working';
    const of = (n, total, word) => `${n.toLocaleString()} of ${total.toLocaleString()} ${word}${total === 1 ? '' : 's'}`;
    sub = byAlbum ? of(run.albumsDone, run.albumsTotal, 'album')
      : run.songsTotal ? of(run.songsRead, run.songsTotal, 'song')
      : of(run.processed, run.total, 'folder');
    // Picked albums are matched in bulk alongside (barcodes, then Apple), said in the reason.
    if (run.reason) sub += ` · ${run.reason}`;
    // Time left from the pace so far, once there is enough of it to go on.
    if (byAlbum) {
      if (coverPace?.runId !== run.runId) coverPace = { runId: run.runId, at: Date.now(), done: run.albumsDone };
      const moved = run.albumsDone - coverPace.done;
      if (moved >= 3) {
        const left = (Date.now() - coverPace.at) / moved * Math.max(0, run.albumsTotal - run.albumsDone) / 1000;
        if (left >= 5) sub += left < 90 ? ` · about ${Math.round(left / 5) * 5} s left` : ` · about ${Math.round(left / 60)} min left`;
      }
    }
  } else if (run.mode === 'Scan') {
    head = run.soft ? `${plural(run.soft, 'album has a soft cover', 'albums have soft covers')}` : 'Every cover is sharp';
    sub = `under ${run.smallerThan} px${run.kept ? ` · ${run.kept} already sharp` : ''}`;
  } else if (run.mode === 'Preview') {
    head = run.upgraded ? `${plural(run.upgraded, 'larger cover')} found` : 'No larger covers found';
    const different = rows.filter(row => row.result === 'found' && row.looksSame === false).length;
    sub = [run.kept ? `${run.kept} with nothing larger` : '',
      different ? `${different} look different, left unpicked` : ''].filter(Boolean).join(' · ');
  } else {
    head = run.upgraded ? `${plural(run.upgraded, 'cover')} replaced` : 'Nothing replaced';
    sub = run.files ? `in ${plural(run.files, 'song')}` : '';
  }
  if (!running && ['Cancelled', 'Interrupted', 'Failed'].includes(run.status)) head = `${head || 'Stopped'} (${run.status.toLowerCase()})`;
  coverEl('cover-head').textContent = head;
  coverEl('cover-sub').textContent = sub;

  coverEl('cover-empty').hidden = run.status !== 'Idle';
  const shown = coverShowAll ? rows : rows.slice(0, COVER_TILE_CAP);
  coverEl('cover-wall').innerHTML = run.undo ? '' : shown.map(row => coverTile(run, row)).join('');
  const more = coverEl('cover-more');
  const lastError = run.errors?.length ? run.errors[run.errors.length - 1] : '';
  more.hidden = !(rows.length > shown.length || lastError);
  more.innerHTML = [
    rows.length > shown.length ? `Showing ${shown.length} of ${rows.length}. <button type="button" class="link-btn" id="cover-show-all">Show all</button> Select all includes the rest.` : '',
    lastError ? `<span class="field-error">${esc(lastError)}</span>` : '',
  ].filter(Boolean).join(' ');
  coverEl('cover-scan').disabled = running;
  renderCoverBar(run);
}

async function loadCoverUpgrade(retry = false) {
  const response = await genreBackfillFetch('/api/admin/covers/upgrade', {}, retry);
  if (!response.ok) {
    if (coverPoll) { clearInterval(coverPoll); coverPoll = null; }
    return null;
  }
  const run = await response.json();
  // Accepted but not started yet: show it as starting, so the page keeps watching and never
  // sits on the last run's results while a new one gets going.
  if (run.busy && run.status !== 'Running') Object.assign(run, { status: 'Running', starting: true, preview: [] });
  const previous = lastCoverRun;
  lastCoverRun = run;

  // Open on the settings of the wall on screen, so what is picked is what it describes.
  if (!coverControlsSet && run.status !== 'Idle' && !run.undo) {
    if (run.scope) coverEl('cover-scope').value = run.scope;
    if (run.smallerThan && [...coverEl('cover-threshold').options].some(o => o.value === String(run.smallerThan)))
      coverEl('cover-threshold').value = String(run.smallerThan);
    coverEl('cover-folder').checked = run.folderCovers;
    document.querySelectorAll('.seg[data-seg-for="cover-scope"], .seg[data-seg-for="cover-threshold"]').forEach(seg => syncSegment(seg, false));
  }
  coverControlsSet = true;
  renderCovers(run);

  if (previous?.status === 'Running' && run.status !== 'Running' && run.status === 'Failed')
    coverNote(run.reason || 'The run stopped.', 'error');

  if (run.status === 'Running') {
    if (!coverPoll) coverPoll = setInterval(() => loadCoverUpgrade(), 1500);
  } else if (coverPoll) {
    clearInterval(coverPoll);
    coverPoll = null;
  }
  return run;
}

async function startCovers(mode, albums = null) {
  const run = lastCoverRun;
  // A pick belongs to the wall it was made on, so it runs with that wall's settings.
  const scope = albums ? run.scope : coverEl('cover-scope').value;
  const folderCovers = albums ? run.folderCovers : coverEl('cover-folder').checked;
  const smallerThan = albums ? run.smallerThan : Number(coverEl('cover-threshold').value || 1000);
  const response = await genreBackfillFetch('/api/admin/covers/upgrade', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ scope, mode, folderCovers, smallerThan, albums }),
  }, true);
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    coverNote(body.error || `Could not start: HTTP ${response.status}`, 'error');
    return;
  }
  await loadCoverUpgrade();
}

function coverPickedIds() {
  return coverRows(lastCoverRun).filter(row => coverPickable(lastCoverRun, row) && coverPicked.has(row.id)).map(row => row.id);
}

coverEl('cover-scan')?.addEventListener('click', () => startCovers('Scan'));
coverEl('cover-scan-first')?.addEventListener('click', () => startCovers('Scan'));
coverEl('cover-folder')?.addEventListener('change', () => {
  if (lastCoverRun?.mode === 'Scan' && !lastCoverRun.undo) coverNote('Scan again to use this.', 'info');
});
coverEl('cover-wall')?.addEventListener('click', event => {
  const tile = event.target.closest('.cover-tile[role="checkbox"]');
  if (!tile || !lastCoverRun) return;
  const id = tile.dataset.coverId;
  if (coverPicked.has(id)) coverPicked.delete(id); else coverPicked.add(id);
  tile.setAttribute('aria-checked', String(coverPicked.has(id)));
  renderCoverBar(lastCoverRun);
});
coverEl('cover-more')?.addEventListener('click', event => {
  if (event.target.id !== 'cover-show-all' || !lastCoverRun) return;
  coverShowAll = true;
  renderCovers(lastCoverRun);
});
coverEl('cover-all')?.addEventListener('click', () => {
  coverPicked = new Set(coverRows(lastCoverRun).filter(row => coverPickable(lastCoverRun, row)).map(row => row.id));
  renderCovers(lastCoverRun);
});
coverEl('cover-none')?.addEventListener('click', () => {
  coverPicked = new Set();
  renderCovers(lastCoverRun);
});
coverEl('cover-go')?.addEventListener('click', async () => {
  const run = lastCoverRun;
  const ids = coverPickedIds();
  if (!run || !ids.length) { coverNote('Pick at least one album first.', 'info'); return; }
  if (run.mode === 'Scan') { await startCovers('Preview', ids); return; }
  if (run.mode === 'Preview') {
    if (!(await askConfirm(`Replace the cover of ${ids.length} album${ids.length === 1 ? '' : 's'}?`, 'Every old cover is kept, so Undo puts them back.', 'Replace covers'))) return;
    await startCovers('Apply', ids);
  }
});
coverEl('cover-stop')?.addEventListener('click', async () => {
  const response = await genreBackfillFetch('/api/admin/covers/upgrade/cancel', { method: 'POST' });
  if (!response.ok) { coverNote('Could not stop.', 'error'); return; }
  coverNote('Stopping after the current album.', 'info');
  await loadCoverUpgrade();
});
coverEl('cover-resume')?.addEventListener('click', async () => {
  const response = await genreBackfillFetch('/api/admin/covers/upgrade/resume', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { coverNote(body.error || 'Could not resume.', 'error'); return; }
  await loadCoverUpgrade();
});
coverEl('cover-undo')?.addEventListener('click', async () => {
  if (!(await askConfirm('Put back the old covers?', 'Every song the upgrades changed gets its old cover back. Songs moved since then stay as they are.', 'Put back covers'))) return;
  const response = await genreBackfillFetch('/api/admin/covers/upgrade/undo', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { coverNote(body.error || 'Could not undo.', 'error'); return; }
  await loadCoverUpgrade();
});

// ---- Library actions -----------------------------------------------------

let libraryActions = [];

const libraryActionLabels = {
  Delete: 'Delete the file and never ask for it again',
  WrongSong: 'Wrong song: replace it, and blacklist the peer that sent it',
  WrongVersion: 'Wrong version: find the plain recording instead',
  BetterQuality: 'Better quality: upgrade only to a larger lossless copy',
  Keep: 'Keep: the track is right, so stop asking. Removes nothing',
};

function normalizeLibraryAction(action = {}) {
  return {
    ...action,
    Action: action.Action ?? action.action ?? '',
    Name: action.Name ?? action.name ?? '',
    Enabled: (action.Enabled ?? action.enabled ?? false) === true,
    // null and 0 mean different things: unset takes the built-in mapping, 0 means no rating
    // ever triggers this. Keep null as null rather than coercing it to a number.
    Rating: action.Rating ?? action.rating ?? null,
  };
}

function renderLibraryActions(actions = libraryActions) {
  const list = document.getElementById('library-action-list');
  if (!list) return;
  libraryActions = (Array.isArray(actions) ? actions : []).map(normalizeLibraryAction);

  list.innerHTML = libraryActions.map((action, index) => `
      <div class="source-priority-row library-action-row${action.Enabled ? '' : ' is-disabled'}" data-index="${index}">
        <span class="source-copy">
          <span class="source-title">${esc(action.Action)}</span>
          <span class="source-detail">${esc(libraryActionLabels[action.Action] || '')}</span>
        </span>
        <input class="set-input" data-action-field="Name" value="${esc(action.Name)}"
               placeholder="playlist name" aria-label="${esc(action.Action)} playlist name" />
        <select class="set-input" data-action-field="Rating" aria-label="${esc(action.Action)} star rating">
          <option value="0"${Number(action.Rating) === 0 ? ' selected' : ''}>no rating</option>
          ${[1, 2, 3, 4, 5].map(star =>
            `<option value="${star}"${Number(action.Rating) === star ? ' selected' : ''}>${star} star${star === 1 ? '' : 's'}</option>`).join('')}
        </select>
        <label class="switch source-kind-switch">
          <input type="checkbox" data-action-field="Enabled" aria-label="Enable ${esc(action.Action)}" ${action.Enabled ? 'checked' : ''} />
          <span class="sw-track"></span><span class="sw-thumb"></span>
        </label>
      </div>`).join('');
  syncLibraryActionsInput(false);
}

function syncLibraryActionsInput(showErrors = true) {
  document.querySelectorAll('#library-action-list .library-action-row').forEach(row => {
    const action = libraryActions[Number(row.dataset.index)];
    if (!action) return;
    row.querySelectorAll('[data-action-field]').forEach(input => {
      const field = input.dataset.actionField;
      if (field === 'Enabled') action.Enabled = input.checked;
      else if (field === 'Rating') action.Rating = Number(input.value);
      else action[field] = input.value;
    });
  });

  const input = document.getElementById('library-actions-json');
  const error = document.getElementById('library-action-error');
  if (!input) return true;

  let message = '';
  const ratings = new Set();
  for (const action of libraryActions) {
    if ((action.Name || '').trim().length > 80) { message = `"${action.Action}" has a name longer than 80 characters.`; break; }
    const rating = Number(action.Rating) || 0;
    // A rating can only mean one thing, so two actions on the same star count is a mistake.
    if (rating > 0 && ratings.has(rating)) { message = `Two actions both use ${rating} star(s).`; break; }
    if (rating > 0) ratings.add(rating);
  }

  if (error) { error.textContent = message; error.hidden = !message || !showErrors; }
  if (message) return false;

  input.value = JSON.stringify(libraryActions);
  return true;
}

function renderLibraryActionUsers(users) {
  const visible = document.getElementById('f-action-users');
  if (visible) visible.value = (Array.isArray(users) ? users : []).join(', ');
  syncLibraryActionUsers();
}

function syncLibraryActionUsers() {
  const visible = document.getElementById('f-action-users');
  const hidden = document.getElementById('action-users-json');
  if (!visible || !hidden) return;
  const entries = visible.value.split(',').map(entry => entry.trim()).filter(Boolean);
  hidden.value = JSON.stringify([...new Set(entries)]);
}

const libraryActionList = document.getElementById('library-action-list');
libraryActionList?.addEventListener('input', () => syncLibraryActionsInput());
libraryActionList?.addEventListener('change', event => {
  if (event.target.matches('[data-action-field="Enabled"]')) { renderLibraryActions(libraryActions); return; }
  syncLibraryActionsInput();
});
document.getElementById('f-action-users')?.addEventListener('input', syncLibraryActionUsers);

// The action history and Octo's questions both sit behind the admin browse session and read the
// same way: fetch, sign in once on a 401, then one row per entry.
async function showSessionTable(holder, path, head, row, summary = () => '') {
  if (!holder) return;
  holder.innerHTML = stateBlock('loading', 'Loading…');
  try {
    let response = await api(path, { credentials: 'same-origin' });
    if (response.status === 401 && await browseAuthenticate(holder)) {
      response = await api(path, { credentials: 'same-origin' });
    }
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();
    const note = summary(body);
    const lead = note ? `<p class="set-info-d">${note}</p>` : '';

    if (!body.entries?.length) { holder.innerHTML = `${lead}${stateBlock('empty', 'Nothing yet.')}`; return; }
    holder.innerHTML = `${lead}
      <div class="config-table">
        <div class="config-row config-row-head genre-change-row">
          ${head.map(label => `<span>${esc(label)}</span>`).join('')}
        </div>
        ${body.entries.map(entry => `
          <div class="config-row genre-change-row">${row(entry)}
          </div>`).join('')}
      </div>`;
  } catch (error) {
    holder.innerHTML = stateBlock('error', error.message);
  }
}

function showLibraryActionsHistory() {
  return showSessionTable(document.getElementById('library-actions-history'), '/api/admin/library-actions',
    ['Track', 'Action', 'Who', 'Result'], entry => `
            <span class="key">${esc(entry.artist)} - ${esc(entry.title)}</span>
            <span class="value">${esc(entry.action)}${entry.dryRun ? ' (rehearsal)' : ''}</span>
            <span class="value">${esc(entry.username)}</span>
            <span class="value">${esc(entry.state)}${entry.detail ? `: ${esc(entry.detail)}` : ''}${entry.blocksDownloads
              ? ` <button type="button" class="btn btn-ghost btn-sm" data-allow-download="${esc(entry.key)}">Allow downloading again</button>`
              : ''}</span>`);
}

document.getElementById('library-actions-refresh')?.addEventListener('click', showLibraryActionsHistory);

// A song removed with Delete from disk is not downloaded again until it is put back, or allowed here.
document.getElementById('library-actions-history')?.addEventListener('click', async event => {
  const button = event.target.closest('[data-allow-download]');
  if (!button) return;
  button.disabled = true;
  try {
    const response = await api('/api/admin/library-actions/allow-download', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: button.dataset.allowDownload }),
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
    toast('It can be downloaded again.');
    await showLibraryActionsHistory();
  } catch (error) {
    button.disabled = false;
    toast(error.message, 'err');
  }
});

const noticeStates = {
  Waiting: 'Waiting its turn',
  Queued: 'Asked',
  Kept: 'Kept',
  Acted: 'Acted on',
  Dismissed: 'Taken out of the playlist',
  Expired: 'Expired',
};

const noticeKinds = { Review: 'Review', Duplicates: 'Duplicate' };

document.getElementById('notices-refresh')?.addEventListener('click', () =>
  showSessionTable(document.getElementById('notices-list'), '/api/admin/notices',
    ['Track', 'Why', 'Who', 'Answer'], entry => `
            <span class="key">${esc(entry.artist)} - ${esc(entry.title)}</span>
            <span class="value">${esc(noticeKinds[entry.kind] || entry.kind)}: ${esc(entry.reason)}${entry.origin === 'LibrarySweep' ? ' (from the library)' : ''}</span>
            <span class="value">${esc(entry.username)}</span>
            <span class="value">${esc(noticeStates[entry.state] || entry.state)}${entry.submitted ? ', sent to AcoustID' : ''}</span>`,
    body => {
      const scan = body.duplicateScan;
      if (!scan) return '';
      const when = new Date(scan.atUtc).toLocaleString();
      return `Last duplicate scan ${esc(when)}: ${scan.tracks} tracks with a recording id, ${scan.groups} group${scan.groups === 1 ? '' : 's'}`
        + (scan.complete ? '.' : ', but the walk did not finish, so nothing was settled.');
    }));

document.getElementById('duplicates-scan')?.addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    const response = await api('/api/admin/duplicates/scan', { method: 'POST' });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
    note(button, 'Scanning now. What it finds shows under Questions.');
  } catch (error) {
    note(button, error.message, 'error');
  } finally {
    button.disabled = false;
  }
});

// ---- Review: the library check (#72) ----
const reviewSweepStates = { Off: 'Off', Paused: 'Paused', Waiting: 'Waiting', Running: 'Checking', Done: 'Up to date' };

async function loadReviewSweep() {
  const status = document.getElementById('review-sweep-status');
  if (!status) return;
  try {
    const response = await api('/api/admin/review-sweep');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const s = await response.json();
    const parts = [`${reviewSweepStates[s.state] || s.state}.`];
    if (s.total > 0) parts.push(`Checked ${s.position} of ${s.total} songs (pass ${s.pass}).`);
    parts.push(`Found ${s.found}, ${s.open} waiting for an answer.`);
    if (s.undecodable > 0) parts.push(`${s.undecodable} could not be decoded.`);
    if (s.keeper) parts.push(`Asking ${s.keeper}.`);
    if (s.reason) parts.push(s.reason);
    status.textContent = parts.join(' ');
    const toggle = document.getElementById('review-sweep-toggle');
    toggle.dataset.paused = s.paused ? 'true' : 'false';
    toggle.querySelector('span').textContent = s.paused ? 'Start' : 'Pause';
    toggle.disabled = s.perHour === 0;
  } catch (error) {
    status.textContent = `Could not read the library check: ${error.message}`;
  }
}

async function reviewSweepPost(button, path, message) {
  button.disabled = true;
  try {
    const response = await api(path, { method: 'POST' });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
    note(button, message);
  } catch (error) {
    note(button, error.message, 'error');
  } finally {
    button.disabled = false;
    await loadReviewSweep();
  }
}

document.getElementById('review-sweep-toggle')?.addEventListener('click', (event) => {
  const button = event.currentTarget;
  const start = button.dataset.paused === 'true';
  reviewSweepPost(button, `/api/admin/review-sweep/${start ? 'start' : 'pause'}`, start ? 'Carrying on.' : 'Paused.');
});
document.getElementById('review-sweep-reset')?.addEventListener('click', async (event) => {
  // Taken before the question: once it is awaited the event no longer says which button it was.
  const button = event.currentTarget;
  if (!(await askConfirm('Check every song again?', 'From the start. Questions you answered recently are not asked again.', 'Start over'))) return;
  reviewSweepPost(button, '/api/admin/review-sweep/reset', 'Starting over.');
});
ready.then(loadReviewSweep);
// Not while the sign-in screen is up: every refused poll would wait behind it and fire at once after.
setInterval(() => { if (document.visibilityState === 'visible' && !gatePromise) loadReviewSweep(); }, 30000);

// Sounds alike: octo-sonic's analysis of every song, on the Last.fm radio page.
const sonicStates = { Off: 'Off', Paused: 'Paused', Waiting: 'Waiting', Running: 'Analyzing', Done: 'Up to date' };

async function loadSonic() {
  const status = document.getElementById('sonic-status');
  if (!status) return;
  try {
    const response = await api('/api/admin/sonic');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const s = await response.json();
    const parts = [`${sonicStates[s.state] || s.state}.`];
    if (s.total > 0) parts.push(`${s.analysed} of ${s.total} songs analyzed (pass ${s.pass}).`);
    if (s.failed > 0) parts.push(`${s.failed} could not be read.`);
    if (s.skipped > 0) parts.push(`${s.skipped} left out: Octo cannot find the file, or the song has no length or runs over 45 minutes.`);
    if (s.reason) parts.push(s.reason);
    status.textContent = parts.join(' ');
    const toggle = document.getElementById('sonic-toggle');
    toggle.dataset.paused = s.paused ? 'true' : 'false';
    toggle.querySelector('span').textContent = s.paused ? 'Start' : 'Pause';
  } catch (error) {
    status.textContent = `Could not read the analysis: ${error.message}`;
  }
}

async function sonicPost(button, path, message) {
  button.disabled = true;
  try {
    const response = await api(path, { method: 'POST' });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
    note(button, message);
  } catch (error) {
    note(button, error.message, 'error');
  } finally {
    button.disabled = false;
    await loadSonic();
  }
}

document.getElementById('sonic-toggle')?.addEventListener('click', (event) => {
  const button = event.currentTarget;
  const start = button.dataset.paused === 'true';
  sonicPost(button, `/api/admin/sonic/${start ? 'start' : 'pause'}`, start ? 'Carrying on.' : 'Paused.');
});
document.getElementById('sonic-reset')?.addEventListener('click', async (event) => {
  // Taken before the question: once it is awaited the event no longer says which button it was.
  const button = event.currentTarget;
  if (!(await askConfirm('Analyze every song again?', 'Octo forgets what every song sounds like and reads them all again, one at a time.', 'Start over'))) return;
  sonicPost(button, '/api/admin/sonic/reset', 'Starting over.');
});
loadSonic();
setInterval(() => { if (document.visibilityState === 'visible') loadSonic(); }, 30000);

// What radio has learned from listening, per source, across listeners.
async function loadRadioOutcomes() {
  const table = document.getElementById('radio-outcomes');
  const summary = document.getElementById('radio-outcomes-summary');
  if (!table || !summary) return;
  try {
    const response = await api('/api/admin/radio-outcomes');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const rows = await response.json();
    const body = table.querySelector('tbody');
    body.replaceChildren(...rows.map(r => {
      const tr = document.createElement('tr');
      const range = r.lowestMultiplier === r.highestMultiplier
        ? `x${r.lowestMultiplier.toFixed(2)}`
        : `x${r.lowestMultiplier.toFixed(2)} to x${r.highestMultiplier.toFixed(2)}`;
      for (const text of [r.name, String(r.plays), `${Math.round(r.keepRate * 100)}%`, range]) {
        const td = document.createElement('td');
        td.textContent = text;
        tr.append(td);
      }
      return tr;
    }));
    table.hidden = rows.length === 0;
    summary.textContent = rows.length === 0
      ? 'Nothing yet: radio learns as songs it suggested are played or skipped.'
      : 'Across every listener. Each listener has their own adjustments; the range shows how far apart they are.';
  } catch (error) {
    summary.textContent = `Could not read what radio has learned: ${error.message}`;
  }
}

document.getElementById('radio-outcomes-reset')?.addEventListener('click', async (event) => {
  // Taken before the question: once it is awaited the event no longer says which button it was.
  const button = event.currentTarget;
  if (!(await askConfirm('Forget what radio learned?', 'Every listener goes back to the base weights, and radio starts learning again.', 'Forget'))) return;
  button.disabled = true;
  try {
    const response = await api('/api/admin/radio-outcomes/reset', { method: 'POST' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    note(button, 'Forgotten.');
  } catch (error) {
    note(button, error.message, 'error');
  } finally {
    button.disabled = false;
    await loadRadioOutcomes();
  }
});
loadRadioOutcomes();
setInterval(() => { if (document.visibilityState === 'visible') loadRadioOutcomes(); }, 30000);

const upgradeOutcomes = { Applied: 'upgraded', Failed: 'no better copy found', Rehearsed: 'dry run',
  Skipped: 'skipped', Unresolved: 'file not found', Nothing: 'nothing left to try' };

async function loadQualityUpgrade() {
  const output = document.getElementById('quality-upgrade-status');
  if (!output) return;
  try {
    const response = await api('/api/admin/quality-upgrade');
    if (!response.ok) { output.textContent = ''; return; }
    const data = await response.json();
    const when = value => new Date(value).toLocaleString();
    const parts = [];
    if (data.perWeek > 0 && data.off) parts.push(`Not running: ${data.off}`);
    if (data.lastRunUtc) parts.push(`Last ran ${when(data.lastRunUtc)}`
      + (data.lastOutcome ? ` (${upgradeOutcomes[data.lastOutcome] || data.lastOutcome}).` : '.'));
    if (data.nextDueUtc) parts.push(`Next one due ${when(data.nextDueUtc)}.`);
    output.textContent = parts.join(' ');
  } catch { output.textContent = ''; }
}

document.getElementById('lidarr-test-connection')?.addEventListener('click', async (event) => {
  const button = event.currentTarget;
  const form = document.getElementById('lidarr-connection-form');
  const baseUrl = document.getElementById('f-lidarr-url')?.value ?? '';
  const apiKey = document.getElementById('f-lidarr-key')?.value ?? '';
  button.disabled = true;
  saveStatus(form, 'Testing…', 'busy');
  try {
    const r = await api('/api/admin/lidarr/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ baseUrl, apiKey }),
    });
    const result = await r.json();
    if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
    populateLidarrOptions(result.options || {});
    saveStatus(form, result.message || 'Connected to Lidarr.');
  } catch (err) {
    saveStatus(form, `Test failed: ${err.message}`, 'error');
  } finally {
    button.disabled = false;
  }
});

function isFieldDirty(el) {
  if (!currentSettings || !el.name?.includes('.')) return false;
  const [section, key] = el.name.split('.');
  const saved = currentSettings?.[section]?.[key];
  let live;
  if (el.type === 'checkbox') live = el.checked;
  else if (el.type === 'number') live = el.value === '' ? null : Number(el.value);
  else live = el.value;
  if ((saved === null || saved === undefined || saved === '') &&
      (live === null || live === undefined || live === '')) return false;
  return saved != live;
}

// Lidarr owns these choices. Keep them disabled until a saved connection can
// supply real values; this avoids persisting a made-up profile id.
async function loadLidarrOptions() {
  const status = document.getElementById('lidarr-options-status');
  const root = document.getElementById('f-lidarr-root');
  const quality = document.getElementById('f-lidarr-quality');
  const metadata = document.getElementById('f-lidarr-metadata');
  if (!root || !quality || !metadata) return;

  if (status) status.textContent = 'Loading choices…';
  [root, quality, metadata].forEach(el => { el.disabled = true; });
  try {
    const r = await api('/api/admin/lidarr/options', { cache: 'no-store' });
    const data = await r.json();
    if (!r.ok) throw new Error(data.error || `HTTP ${r.status}`);

    populateLidarrOptions(data);
  } catch (err) {
    if (status) status.textContent = `Could not load choices: ${err.message}`;
  }
}

function populateLidarrOptions(data) {
  const root = document.getElementById('f-lidarr-root');
  if (!root) return;
  markCleanIfWasClean(root.closest('form'), () => fillLidarrOptions(data));
}

function fillLidarrOptions(data) {
  const status = document.getElementById('lidarr-options-status');
  const root = document.getElementById('f-lidarr-root');
  const quality = document.getElementById('f-lidarr-quality');
  const metadata = document.getElementById('f-lidarr-metadata');
  if (!root || !quality || !metadata) return;
  const selectedRoot = root.value || currentSettings?.Lidarr?.RootFolderPath || '';
  const selectedQuality = quality.value !== '0'
    ? quality.value : String(currentSettings?.Lidarr?.QualityProfileId ?? 0);
  const selectedMetadata = metadata.value !== '0'
    ? metadata.value : String(currentSettings?.Lidarr?.MetadataProfileId ?? 0);
  root.innerHTML = '<option value="">Choose a root folder</option>' +
    (data.rootFolders || []).map(x => `<option value="${esc(x.path)}">${esc(x.path)}</option>`).join('');
  quality.innerHTML = '<option value="0">Choose a quality profile</option>' +
    (data.qualityProfiles || []).map(x => `<option value="${x.id}">${esc(x.name)}</option>`).join('');
  metadata.innerHTML = '<option value="0">Choose a metadata profile</option>' +
    (data.metadataProfiles || []).map(x => `<option value="${x.id}">${esc(x.name)}</option>`).join('');
  root.value = selectedRoot;
  quality.value = selectedQuality;
  metadata.value = selectedMetadata;
  [root, quality, metadata].forEach(el => { el.disabled = false; });
  const empty = [];
  if (!(data.rootFolders || []).length) empty.push('root folders');
  if (!(data.qualityProfiles || []).length) empty.push('quality profiles');
  if (!(data.metadataProfiles || []).length) empty.push('metadata profiles');
  if (status) status.textContent = empty.length
    ? `Connected, but Lidarr returned no ${empty.join(', ')}.`
    : 'Connected. Choices loaded from Lidarr.';
}

// ────────────────────────────────────────────────────────────────
// Raw config editor
// ────────────────────────────────────────────────────────────────
const rawEditor = document.getElementById('raw-editor');
const rawError  = document.getElementById('raw-error');
const rawForm   = document.getElementById('raw-form');
// Edits in the Raw editor survive switching away and back; only an explicit reload replaces them.
let rawDirty = false;

async function loadRawConfig(force = false) {
  if (!rawEditor) return;
  if (rawDirty && !force) return;
  try {
    const r = await api('/api/admin/raw-config', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    rawEditor.value = await r.text();
    rawError.hidden = true;
    rawDirty = false;
  } catch (e) {
    rawError.textContent = `Could not load the settings file: ${e.message}`;
    rawError.hidden = false;
  }
}

if (rawForm) {
  // Live JSON validation as the user types, so errors show before save.
  rawEditor.addEventListener('input', () => {
    rawDirty = true;
    const val = rawEditor.value.trim();
    if (!val) { rawError.hidden = true; return; }
    try {
      const parsed = JSON.parse(val);
      if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
        throw new Error('top level must be an object');
      }
      rawError.hidden = true;
    } catch (e) {
      rawError.textContent = e.message;
      rawError.hidden = false;
    }
  });

  const rawSavedStatus = document.getElementById('raw-saved-status');

  rawForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const val = rawEditor.value;
    try {
      JSON.parse(val);
    } catch (err) {
      rawError.textContent = err.message;
      rawError.hidden = false;
      rawEditor.focus();
      return;
    }
    if (!(await askConfirm('Replace settings.json?', 'With exactly this text. Every value shown here, including ones that came from environment variables, is written into the file and overrides .env from now on. Every settings card reloads afterwards.', 'Replace settings.json', true))) return;

    const submit = rawForm.querySelector('button[type="submit"]');
    submit.disabled = true;
    if (rawSavedStatus) rawSavedStatus.textContent = 'Saving…';

    try {
      const r = await api('/api/admin/raw-config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: val,
      });
      const result = await r.json();
      if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
      if (rawSavedStatus) rawSavedStatus.innerHTML = `${icon('i-check-circle-fill')}<span>Saved at ${esc(new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }))}, ${esc(result.bytes)} bytes</span>`;
      rawDirty = false;
      // Refresh the form-by-form view so any open tab reflects changes.
      await loadSettings();
    } catch (err) {
      if (rawSavedStatus) rawSavedStatus.textContent = '';
      rawError.textContent = `Not saved: ${err.message}`;
      rawError.hidden = false;
    } finally {
      submit.disabled = false;
    }
  });

  document.getElementById('raw-reload')?.addEventListener('click', async () => {
    if (rawDirty && !(await askConfirm('Discard your edits?', 'settings.json is read again from disk.', 'Discard edits', true))) return;
    await loadRawConfig(true);
    if (rawSavedStatus) rawSavedStatus.innerHTML = `${icon('i-check-circle-fill')}<span>Reloaded from disk</span>`;
  });
}

// ────────────────────────────────────────────────────────────────
// Config sources table
// ────────────────────────────────────────────────────────────────
const configTable = document.getElementById('config-table');

async function loadConfigSources() {
  if (!configTable) return;
  try {
    const r = await api('/api/admin/config-sources');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();
    // Wipe everything except the header row.
    configTable.querySelectorAll('.config-row:not(.config-row-head), .config-loading')
      .forEach(el => el.remove());
    for (const row of data.keys) {
      const div = document.createElement('div');
      div.className = 'config-row';
      const valueClass = row.IsSecret ? 'value secret' : (row.Value === '' ? 'value empty' : 'value');
      const valueText = row.Value === '' ? '(unset)' : row.Value;
      div.innerHTML = `
        <span class="key">${escapeHtml(row.Key)}</span>
        <span class="${valueClass}">${escapeHtml(valueText)}</span>
      `;
      configTable.appendChild(div);
    }
  } catch (e) {
    const loading = configTable.querySelector('.config-loading');
    if (loading) loading.innerHTML = stateBlock('error', `Could not load the values: ${e.message}`);
  }
}

// Raw config, Config sources and radio status reload in activateTab whenever their pane opens.

// ────────────────────────────────────────────────────────────────
// Restart button
// ────────────────────────────────────────────────────────────────
document.getElementById('restart-btn').addEventListener('click', async () => {
  const unsavedCards = document.querySelectorAll('form.unsaved').length;
  const unsavedNote = unsavedCards
    ? `\n\nYou have unsaved changes on ${unsavedCards} card${unsavedCards === 1 ? '' : 's'}; restarting discards them.`
    : '';
  if (!(await askConfirm('Restart Octo?', `Requests in flight are dropped, and Octo is back in 5 to 10 seconds.${unsavedNote}`, 'Restart', true))) return;
  const btn = document.getElementById('restart-btn');
  const label = btn.querySelector('span');
  btn.disabled = true;
  if (label) label.textContent = 'Restarting…';
  toast('Restart triggered. Waiting for service…');

  try { await api('/api/admin/restart', { method: 'POST' }); }
  catch { /* expected: the connection drops */ }

  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 1500));
    try {
      const r = await api('/api/admin/status', { cache: 'no-store' });
      if (r.ok) {
        toast('Octo is back online.', 'ok');
        btn.disabled = false;
        if (label) label.textContent = 'Restart';
        await loadSettings();
        await refreshStatus();
        return;
      }
    } catch { /* keep polling */ }
  }
  toast('Service did not come back within 60s.', 'error');
  btn.disabled = false;
  if (label) label.textContent = 'Restart';
});

// ────────────────────────────────────────────────────────────────
// Discovery-off banner: loud when no Last.fm key is set
// ────────────────────────────────────────────────────────────────
function updateDiscoveryBanner() {
  const banner = document.getElementById('discovery-off-banner');
  const keyInput = document.getElementById('f-lastfm-key');
  if (!banner || !keyInput) return;
  banner.hidden = !!keyInput.value.trim();
}
document.getElementById('f-lastfm-key')?.addEventListener('input', updateDiscoveryBanner);

// Personalized + pinned Radio remains part of the Last.fm pane. The editor keeps
// each original object intact so fields introduced by a newer Octo are not erased
// when an older browser session edits a row. Station display order belongs to each
// Subsonic client, so the admin UI intentionally does not promise reordering.
let radioDiscoveryStations = [];
const radioPresets = {
  rock: { Name: 'Rock Discovery', Tags: ['rock', 'alternative rock'] },
  jazz: { Name: 'Jazz Discovery', Tags: ['jazz', 'contemporary jazz'] },
  electronic: { Name: 'Electronic Discovery', Tags: ['electronic', 'electronica', 'idm'] },
};

function radioId() {
  return (crypto.randomUUID?.() || `${Date.now()}${Math.random()}`).replace(/[^a-z0-9]/gi, '').slice(0, 24).toLowerCase();
}

function normalizeRadioStation(item = {}) {
  return { ...item, Id: String(item.Id ?? item.id ?? radioId()), Name: String(item.Name ?? item.name ?? ''),
    Enabled: Boolean(item.Enabled ?? item.enabled ?? true),
    Tags: Array.isArray(item.Tags ?? item.tags) ? [...(item.Tags ?? item.tags)] : [] };
}

function renderRadioDiscovery(stations = radioDiscoveryStations) {
  const list = document.getElementById('radio-discovery-list');
  if (!list) return;
  radioDiscoveryStations = (Array.isArray(stations) ? stations : []).map(normalizeRadioStation);
  list.innerHTML = radioDiscoveryStations.length ? radioDiscoveryStations.map((station, index) => `
    <div class="radio-discovery-row${station.Enabled ? '' : ' is-disabled'}" data-index="${index}">
      <div class="radio-discovery-fields">
        <label><span>Station name</span><input class="set-input" data-radio-field="Name" value="${escapeHtml(station.Name)}" maxlength="100" /></label>
        <label><span>Last.fm tags</span><input class="set-input" data-radio-field="Tags" value="${escapeHtml(station.Tags.join(', '))}" placeholder="electronic, electronica, idm" /></label>
      </div>
      <label class="switch" title="Show this station"><input type="checkbox" data-radio-field="Enabled" ${station.Enabled ? 'checked' : ''} aria-label="Enable ${escapeHtml(station.Name || 'station')}" /><span class="sw-track"></span><span class="sw-thumb"></span></label>
      <div class="radio-row-actions" role="group" aria-label="Actions for ${escapeHtml(station.Name || 'station')}">
        <button class="btn btn-ghost" type="button" data-radio-action="remove">Remove</button>
      </div>
    </div>`).join('') : '<div class="radio-empty">No pinned categories yet. Add a preset or create a custom tag station.</div>';
  syncRadioDiscoveryInput(false);
}

function readRadioDiscoveryRows() {
  document.querySelectorAll('.radio-discovery-row').forEach((row, index) => {
    const station = radioDiscoveryStations[index]; if (!station) return;
    station.Name = row.querySelector('[data-radio-field="Name"]')?.value.trim() || '';
    station.Enabled = Boolean(row.querySelector('[data-radio-field="Enabled"]')?.checked);
    station.Tags = (row.querySelector('[data-radio-field="Tags"]')?.value || '').split(',')
      .map(tag => tag.trim().toLowerCase().replace(/\s+/g, ' ')).filter(Boolean).filter((tag, i, all) => all.indexOf(tag) === i);
  });
}

function syncRadioDiscoveryInput(showErrors = true) {
  readRadioDiscoveryRows(); const errors = []; const names = new Set();
  if (radioDiscoveryStations.length > 12) errors.push('Use no more than 12 pinned stations.');
  radioDiscoveryStations.forEach((station, index) => {
    const label = `Station ${index + 1}`; const nameKey = station.Name.toLowerCase();
    if (!station.Name) errors.push(`${label} needs a name.`);
    else if (names.has(nameKey)) errors.push(`Station names must be unique (${station.Name}).`);
    names.add(nameKey);
    if (!station.Tags.length) errors.push(`${station.Name || label} needs at least one tag.`);
    if (station.Tags.length > 5) errors.push(`${station.Name || label} has more than five tags.`);
  });
  const error = document.getElementById('radio-discovery-error');
  if (error) { error.hidden = !showErrors || errors.length === 0; error.textContent = errors.join(' '); }
  if (errors.length && showErrors) { document.querySelector('.radio-discovery-row .set-input')?.focus(); return false; }
  const input = document.getElementById('radio-discovery-json'); if (input) input.value = JSON.stringify(radioDiscoveryStations);
  return true;
}

document.querySelectorAll('[data-radio-preset]').forEach(button => button.addEventListener('click', () => {
  readRadioDiscoveryRows();
  if (radioDiscoveryStations.length >= 12) return toast('Pinned discovery supports up to 12 stations.', 'error');
  radioDiscoveryStations.push(normalizeRadioStation({ Id: radioId(), Enabled: true, ...radioPresets[button.dataset.radioPreset] })); renderRadioDiscovery();
}));
document.getElementById('radio-add-custom')?.addEventListener('click', () => {
  readRadioDiscoveryRows();
  if (radioDiscoveryStations.length >= 12) return toast('Pinned discovery supports up to 12 stations.', 'error');
  radioDiscoveryStations.push(normalizeRadioStation({ Id: radioId(), Name: 'Custom Discovery', Enabled: true, Tags: [] })); renderRadioDiscovery();
  document.querySelector('.radio-discovery-row:last-child [data-radio-field="Name"]')?.focus();
});
document.getElementById('radio-discovery-list')?.addEventListener('input', () => syncRadioDiscoveryInput(false));
document.getElementById('radio-discovery-list')?.addEventListener('change', event => {
  if (!event.target.matches('[data-radio-field="Enabled"]')) return;
  event.target.closest('.radio-discovery-row')?.classList.toggle('is-disabled', !event.target.checked);
});
document.getElementById('radio-discovery-list')?.addEventListener('click', async event => {
  const button = event.target.closest('[data-radio-action]'); const row = button?.closest('.radio-discovery-row');
  if (!button || !row) return; readRadioDiscoveryRows(); const index = Number(row.dataset.index);
  if (button.dataset.radioAction === 'remove') { if (!(await askConfirm(`Remove “${radioDiscoveryStations[index].Name}”?`, 'Listening history and downloaded music are untouched.', 'Remove', true))) return; radioDiscoveryStations.splice(index, 1); }
  renderRadioDiscovery();
});

async function loadRadioStatus() {
  const output = document.getElementById('radio-status'); const select = document.getElementById('radio-user');
  if (!output || !select) return; output.textContent = 'Loading radio status…';
  try {
    const query = select.value ? `?user=${encodeURIComponent(select.value)}` : '';
    const response = await api(`/api/admin/lastfm/radio${query}`); if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json(); const previous = select.value; const users = data.users || [];
    select.innerHTML = users.map(user => `<option value="${escapeHtml(user.username)}">${escapeHtml(user.username)}</option>`).join('');
    select.value = users.some(user => user.username === previous) ? previous : (data.selectedUser || '');
    // Most Octo installs have one Navidrome account. Keep the profile selector out
    // of that path, but reveal it when the proxy has genuinely observed multiple
    // authenticated listeners whose histories must remain isolated.
    select.hidden = users.length < 2;
    select.disabled = users.length === 0;
    const listenerSummary = document.getElementById('radio-listener-summary');
    if (listenerSummary) listenerSummary.textContent = users.length === 0
      ? 'Appears after a Subsonic client signs in through Octo.'
      : users.length === 1
        ? `${users[0].username} · Radio follows this authenticated Navidrome account.`
        : 'Choose which authenticated Navidrome account to inspect.';
    const resetButton = document.getElementById('radio-reset');
    if (resetButton) resetButton.disabled = users.length === 0;
    const learning = data.learning;
    const stateMessage = !data.enabled ? 'Radio is disabled. Existing history and snapshots are preserved.'
      : !data.hasApiKey ? 'Last.fm key missing. Starter/local fallback remains available; recommendation refresh is degraded.'
      : !learning ? 'Waiting for the first authenticated completed scrobble.'
      : learning.needed > 0 ? `${learning.plays} completed plays learned · ${learning.needed} more before Your Mix.`
      : learning.refreshing ? 'Refreshing now. The last good snapshots remain playable.'
      : learning.lastRefreshError ? `Last refresh failed: ${learning.lastRefreshError}` : `${learning.plays} completed plays learned.`;
    output.innerHTML = `<p class="radio-state-copy">${escapeHtml(stateMessage)}</p>` + ((data.stations || []).length
      ? `<div class="radio-station-grid">${data.stations.map(station => `<article class="radio-station-item"><div><strong>${escapeHtml(station.name)}</strong><span>${escapeHtml(station.kind)} · ${station.trackCount} tracks</span></div><p>${(station.preview || []).map(track => `${escapeHtml(track.artist)} — ${escapeHtml(track.title)}`).join(' · ') || 'Snapshot has no preview yet.'}</p></article>`).join('')}</div>`
      : '<div class="radio-empty">No ready snapshots yet. Stations are offered automatically as listening signals arrive.</div>');
  } catch (error) { output.textContent = `Radio status unavailable: ${error.message}`; }
}
document.getElementById('radio-user')?.addEventListener('change', loadRadioStatus);
document.getElementById('radio-reset')?.addEventListener('click', async event => {
  const user = document.getElementById('radio-user')?.value; if (!user || !(await askConfirm(`Reset Radio history for “${user}”?`, 'Downloaded music is not removed.', 'Reset history', true))) return;
  const button = event.currentTarget;
  button.disabled = true;
  try { const response = await api(`/api/admin/lastfm/radio/history?user=${encodeURIComponent(user)}`, { method: 'DELETE' }); const data = await response.json();
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`); toast(data.message || 'Radio history reset.'); await loadRadioStatus();
  } catch (error) { toast(`Reset failed: ${error.message}`, 'error'); } finally { button.disabled = false; }
});

// ────────────────────────────────────────────────────────────────
// Last.fm account: check the API key and shared secret with Last.fm
// ────────────────────────────────────────────────────────────────
const LFM_KEY_TEXT = {
  ok: ['ok', 'Last.fm knows this key.'],
  invalid: ['bad', "Last.fm doesn't know this key. Copy the API key from your app's page on Last.fm."],
  unreachable: ['warn', "Couldn't reach Last.fm to check this key."],
};
const LFM_SECRET_TEXT = {
  ok: ['ok', 'Matches this key. Scrobbling is ready.'],
  invalid: ['bad', "This isn't the shared secret of that API key. Copy both from the same app on Last.fm."],
  'same-as-key': ['bad', 'This is the API key again. The shared secret is the other value, on the line under it.'],
  missing: ['warn', 'Add it to scrobble outside plays. Discovery and radio work without it.'],
};

function showLastFmCheck(id, entry) {
  const el = document.getElementById(id);
  if (!el) return;
  el.hidden = !entry;
  if (!entry) return;
  el.className = `lfm-check lfm-check-${entry[0]}`;
  el.textContent = entry[1];
}

function renderLastFmCheck(check) {
  showLastFmCheck('lfm-key-check', LFM_KEY_TEXT[check.key]);
  showLastFmCheck('lfm-secret-check', check.key === 'ok' ? LFM_SECRET_TEXT[check.secret] : null);
  const guide = document.getElementById('lfm-setup-guide');
  if (guide) guide.hidden = check.key === 'ok' && check.secret === 'ok';
}

async function checkLastFmAccount(apiKey, apiSecret) {
  const r = await api('/api/admin/lastfm/check', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ apiKey, apiSecret }),
  });
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  return r.json();
}

// On opening the tab: what the saved values are worth, without asking Last.fm when nothing is saved.
async function loadLastFmAccount() {
  if (!currentSettings) return; // the settings load calls this once they are in
  const key = currentSettings.LastFm?.ApiKey;
  if (!key) { renderLastFmCheck({ key: 'missing', secret: 'missing' }); return; }
  try { renderLastFmCheck(await checkLastFmAccount(null, null)); } catch { /* the notes stay as they were */ }
}

// Called by the form's Save before anything is written. False stops the save.
async function lastFmAccountMaySave(form) {
  const key = form.querySelector('[name="LastFm.ApiKey"]')?.value.trim() || '';
  const secret = form.querySelector('[name="LastFm.ApiSecret"]')?.value.trim() || '';
  if (!key) { renderLastFmCheck({ key: 'missing', secret: 'missing' }); return true; }
  let check;
  try { check = await checkLastFmAccount(key, secret); } catch { return true; }
  renderLastFmCheck(check);
  if (check.key === 'invalid' || ['invalid', 'same-as-key'].includes(check.secret)) {
    toast('Nothing saved. Last.fm did not accept these; see the note under each field.', 'error');
    return false;
  }
  return true;
}

// ────────────────────────────────────────────────────────────────
// Last.fm scrobbling: connect each Navidrome user to their own Last.fm
// ────────────────────────────────────────────────────────────────
// Connect opens Last.fm in a new tab, and this page asks Octo every few seconds whether the
// approval has happened, so there is nothing to come back and press. The link lives on the
// server while it waits, so a reload picks the wait up again.
const LFM_POLL_MS = 3000;
const lfmPolls = new Map();      // user (lower case) -> timer
const lfmTabs = new Map();       // user (lower case) -> the tab this page opened for them
const lfmChecking = new Set();   // users with a check out right now
let lfmLastUsers = [];

function lfmAgo(iso) {
  const seconds = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (seconds < 90) return 'just now';
  if (seconds < 3600) return `${Math.round(seconds / 60)} min ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)} h ago`;
  return new Date(iso).toLocaleDateString();
}

async function loadLastFmScrobbling() {
  const list = document.getElementById('lfm-scrobble-users');
  const state = document.getElementById('lfm-scrobble-state');
  if (!list || !state) return;
  try {
    const r = await api('/api/admin/lastfm/scrobble', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const d = await r.json();
    const ready = d.hasApiKey && d.hasApiSecret;
    state.hidden = d.available && ready && d.enabled;
    state.className = `lfm-scrobble-state notice ${ready ? '' : 'notice-warn'}`;
    state.textContent = !d.available ? 'Last.fm scrobbling is not available in this build.'
      : !d.hasApiKey ? 'Save a Last.fm API key and shared secret above to connect listeners.'
      : !d.hasApiSecret ? 'Save the shared secret above to connect listeners. Last.fm needs it to take plays from Octo.'
      : 'Scrobbling is switched off. Connected listeners stay connected.';
    const users = d.users || [];
    lfmLastUsers = users;
    list.innerHTML = users.length ? users.map(u => lfmRow(u, ready)).join('')
      : '<div class="radio-empty">Listeners appear here once they have used Octo. You can also connect one by name below.</div>';
    const add = document.getElementById('lfm-scrobble-add');
    if (add) add.disabled = !ready;
    // Keep waiting on every Connect the server still holds, and stop for the rest.
    const waiting = new Set(users.filter(u => u.awaitingApproval && !u.connected).map(u => u.user.toLowerCase()));
    for (const u of users) if (waiting.has(u.user.toLowerCase())) lfmWatch(u.user);
    for (const key of [...lfmPolls.keys()]) if (!waiting.has(key)) lfmStopWatching(key);
  } catch (e) {
    list.innerHTML = `<div class="radio-empty">Scrobbling status unavailable: ${escapeHtml(e.message)}</div>`;
  }
}

function lfmRow(u, ready) {
  const name = escapeHtml(u.user);
  const waiting = u.awaitingApproval && !u.connected && u.approvalUrl;
  let detail;
  let actions;
  if (u.connected) {
    const sent = u.lastSent
      ? `<span class="set-info-d">Last sent: ${escapeHtml(u.lastSent.title)} by ${escapeHtml(u.lastSent.artist)}, ${lfmAgo(u.lastSent.playedAtUtc)}.</span>`
      : '';
    detail = `<span class="set-info-d lfm-state"><span class="status-dot ok"></span><span>Connected as <strong>${escapeHtml(u.lastFmUser || 'a Last.fm account')}</strong>.</span></span>${sent}`;
    actions = `<button class="btn btn-ghost" type="button" data-lfm-action="disconnect" data-user="${name}">Disconnect</button>`;
  } else if (waiting) {
    detail = `<span class="set-info-d lfm-state"><span class="status-dot pending"></span><span>Waiting for Last.fm. Allow access in the tab that opened; this updates by itself.</span></span>
      <span class="set-info-d lfm-hint">Signed in to Last.fm as someone else? Sign in as ${name} there first, or copy the link and send it to them.</span>`;
    actions = `<a class="btn btn-primary" href="${escapeHtml(u.approvalUrl)}" target="lastfm-${name}" rel="noopener">Open Last.fm</a>
      <button class="btn btn-ghost" type="button" data-lfm-action="copy" data-user="${name}">Copy link</button>
      <button class="btn btn-ghost" type="button" data-lfm-action="cancel" data-user="${name}">Cancel</button>`;
  } else {
    detail = '<span class="set-info-d">Not connected.</span>';
    actions = `<button class="btn" type="button" data-lfm-action="connect" data-user="${name}" ${ready ? '' : 'disabled title="Save the API key and shared secret first"'}>Connect</button>`;
  }
  const notice = u.notice ? `<span class="set-info-d lfm-user-notice">${escapeHtml(u.notice)}</span>` : '';
  return `<div class="set-row lfm-user"><div class="set-info"><span class="set-info-t">${name}</span>${detail}${notice}</div><div class="set-ctrl lfm-actions">${actions}</div></div>`;
}

function lfmWatch(user) {
  const key = user.toLowerCase();
  if (lfmPolls.has(key)) return;
  lfmPolls.set(key, setInterval(() => lfmTryFinish(user), LFM_POLL_MS));
}

function lfmStopWatching(key) {
  clearInterval(lfmPolls.get(key));
  lfmPolls.delete(key);
}

// One quiet check: 409 means Last.fm has not seen the approval yet, so keep waiting.
async function lfmTryFinish(user) {
  const key = user.toLowerCase();
  if (lfmChecking.has(key)) return;
  lfmChecking.add(key);
  try {
    const r = await api('/api/admin/lastfm/scrobble/finish', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ user }),
    });
    if (r.status === 409) return;
    const d = await r.json().catch(() => ({}));
    lfmStopWatching(key);
    const tab = lfmTabs.get(key);
    lfmTabs.delete(key);
    if (r.ok) {
      try { tab?.close(); } catch { /* not ours to close any more */ }
      toast(`${user} is connected to Last.fm as ${d.lastFmUser || 'their account'}.`, 'ok');
    } else {
      toast(d.error || `Connecting ${user} failed.`, 'error');
    }
    await loadLastFmScrobbling();
  } catch {
    // A check that did not get through is tried again on the next tick.
  } finally {
    lfmChecking.delete(key);
  }
}

async function lastFmScrobbleAction(action, user, button) {
  if (!user) return;
  const key = user.toLowerCase();
  if (action === 'copy') {
    const url = lfmLastUsers.find(u => u.user.toLowerCase() === key)?.approvalUrl;
    if (!url) return;
    try { await navigator.clipboard.writeText(url); toast('Link copied. It works for an hour.', 'ok'); }
    catch { await askDialog({ title: 'Copy this link', message: 'It works for an hour.', confirm: 'Done', cancel: null, input: { value: url, readOnly: true } }); }
    return;
  }
  if (action === 'disconnect'
      && !(await askConfirm(`Stop scrobbling outside plays for “${user}”?`, 'Their Last.fm history is untouched.', 'Stop scrobbling', true))) return;
  // The tab has to open inside the click or the browser blocks it; Last.fm's page goes into it
  // once Octo has the link.
  let tab = null;
  if (action === 'connect') {
    tab = window.open('', `lastfm-${user}`);
    try {
      tab?.document.write('<title>Last.fm</title><p style="font:15px system-ui;padding:32px;color:#555">Opening Last.fm…</p>');
    } catch { /* already showing Last.fm */ }
  }
  if (button) button.disabled = true;
  try {
    const r = await api(`/api/admin/lastfm/scrobble/${action}`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ user }),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || `HTTP ${r.status}`);
    if (action === 'connect') {
      if (tab && !tab.closed) {
        // Last.fm's page gets no hold on this one.
        try { tab.opener = null; } catch { /* ignore */ }
        tab.location.href = d.url;
        lfmTabs.set(key, tab);
      } else {
        toast('Your browser kept the Last.fm tab from opening. Use Open Last.fm.', 'error');
      }
      lfmWatch(user);
    } else if (action === 'cancel') {
      lfmStopWatching(key);
      try { lfmTabs.get(key)?.close(); } catch { /* ignore */ }
      lfmTabs.delete(key);
    } else if (action === 'disconnect') {
      toast(d.message || 'Disconnected.', 'ok');
    }
    await loadLastFmScrobbling();
  } catch (e) {
    try { tab?.close(); } catch { /* ignore */ }
    const verb = { disconnect: 'Disconnect', cancel: 'Cancel' }[action] || 'Connect';
    toast(`${verb} failed: ${e.message}`, 'error');
  } finally {
    if (button) button.disabled = false;
  }
}

document.getElementById('lfm-scrobble-users')?.addEventListener('click', event => {
  const button = event.target.closest('[data-lfm-action]');
  if (button) lastFmScrobbleAction(button.dataset.lfmAction, button.dataset.user, button);
});
document.getElementById('lfm-scrobble-add')?.addEventListener('click', async event => {
  const input = document.getElementById('lfm-scrobble-new-user');
  const user = input?.value.trim();
  if (!user) { input?.focus(); return; }
  await lastFmScrobbleAction('connect', user, event.currentTarget);
  if (lfmPolls.has(user.toLowerCase())) input.value = '';
});
// Coming back from the Last.fm tab: check straight away rather than on the next tick.
function lfmCheckNow() {
  for (const u of lfmLastUsers) if (lfmPolls.has(u.user.toLowerCase())) lfmTryFinish(u.user);
}
window.addEventListener('focus', lfmCheckNow);
document.addEventListener('visibilitychange', () => { if (!document.hidden) lfmCheckNow(); });

// ────────────────────────────────────────────────────────────────
// Notifications: send a test through every configured transport
// ────────────────────────────────────────────────────────────────
document.getElementById('btn-check-listenbrainz')?.addEventListener('click', async () => {
  const box = document.getElementById('check-listenbrainz-result');
  const btn = document.getElementById('btn-check-listenbrainz');
  if (!box) return;
  box.hidden = false;
  box.textContent = 'Checking…';
  btn.disabled = true;
  try {
    // Check what is typed, saved or not, so a typo is caught before Save.
    const typed = document.getElementById('f-lb-token')?.value?.trim() ?? '';
    const r = await api('/api/admin/listenbrainz/validate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token: typed || null }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `HTTP ${r.status}`);
    box.textContent = !d.configured ? d.detail : d.valid ? `Valid · ${d.userName}` : `Not valid: ${d.detail}`;
    box.classList.toggle('notice-error', !d.valid);
  } catch (e) {
    box.textContent = `Check failed: ${e.message}`;
    box.classList.add('notice-error');
  } finally {
    btn.disabled = false;
  }
});

document.getElementById('btn-test-notification')?.addEventListener('click', async () => {
  const box = document.getElementById('test-notification-result');
  const btn = document.getElementById('btn-test-notification');
  if (!box) return;
  box.hidden = false;
  box.textContent = 'Sending…';
  btn.disabled = true;
  try {
    const r = await api('/api/admin/test-notification', { method: 'POST' });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || `HTTP ${r.status}`);
    const parts = (d.results || []).map(s =>
      `${s.sink}: ${!s.configured ? 'not configured' : s.ok ? 'OK' : 'failed: ' + s.detail}`);
    box.textContent = parts.length ? parts.join('  ·  ') : 'No transports registered.';
  } catch (e) {
    box.textContent = 'Test failed: ' + (e?.message || 'unknown error');
  } finally {
    btn.disabled = false;
  }
});

// ────────────────────────────────────────────────────────────────
// Library status, library picker, and the download-folder browser
//
// Octo fronts Navidrome, so Navidrome's library is the source of truth for where
// downloads belong. The status line states the whole chain (what Navidrome
// reports, whether Octo can see it, what is therefore in effect) because when
// that chain breaks the symptom is silent: files download fine and never appear.
// ────────────────────────────────────────────────────────────────
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

async function refreshLibraryStatus() {
  const row = document.getElementById('library-status-row');
  const out = document.getElementById('library-status');
  const pickRow = document.getElementById('library-pick-row');
  const pick = document.getElementById('f-library-path');
  if (!row || !out) return;
  try {
    const r = await api('/api/admin/library-status', { cache: 'no-store' });
    if (!r.ok) return;
    const s = await r.json();
    effectiveLibraryPath = s.effectiveDownloadPath || '';
    lastLibraryStatus = s;
    renderSetupChecklist();

    const bits = [];
    if (s.navidromeReports) {
      bits.push(s.visibleToOcto
        ? `Navidrome's library is <code>${esc(s.navidromeReports)}</code>, and Octo can see it.`
        : `Navidrome reports <code>${esc(s.navidromeReports)}</code>, but <strong>Octo cannot see that path</strong>. Mount it into Octo's container, or set Navidrome's remotePath, or pick a folder below.`);
    } else if (s.autoDetect) {
      bits.push('Waiting on Navidrome to report its music folder. Until then the path below is used.');
    } else {
      bits.push("Following Navidrome's folder is off, so the download path below is used as it is.");
    }
    bits.push(`Downloads go to <code>${esc(s.effectiveDownloadPath || '(unset)')}</code>${s.writable ? '' : ', but <strong>Octo cannot write there</strong>'}.`);
    if (!s.rescanAuthenticated) {
      bits.push('No Navidrome admin identity yet, so the rescan after a download may not run. Set admin credentials, or sign in once from a client.');
    }
    out.innerHTML = bits.join(' ');
    row.hidden = false;

    // Only ask which library when there is genuinely a choice to make.
    const libs = s.libraries || [];
    if (pick && pickRow && libs.length > 1) {
      markCleanIfWasClean(pick.closest('form'), () => {
        pick.innerHTML = [`<option value="">First library Navidrome reports</option>`]
          .concat(libs.map(l => {
            const label = `${l.name || l.folder}${l.visible ? '' : ' (not visible to Octo)'}`;
            return `<option value="${esc(l.folder)}">${esc(label)}</option>`;
          })).join('');
        pick.value = s.pinnedLibraryPath || '';
        pick.disabled = false;
        pickRow.hidden = false;
      });
    } else if (pick) {
      // With one library there is nothing to choose, and an empty select would save "" over a
      // pinned library. Disabled fields are not saved.
      pick.disabled = true;
    }
  } catch { /* status is informational; never block the settings UI on it */ }
}
ready.then(refreshLibraryStatus);

// ── Browse for a download folder ────────────────────────────────
// The endpoint requires a Navidrome admin, because an unauthenticated directory
// lister would turn every Octo install into one. The token lives in memory only:
// not sessionStorage, so it dies with the tab.
// The session lives in an HttpOnly cookie the server sets, so it survives a page
// reload and this script never holds it (and could not read it if it tried).
// Nothing about the sign-in is stored client-side, least of all the password.
// Where Navidrome says its library is. Used as the browser's starting point when
// the path field is empty, so it opens on the folder Octo is actually using
// rather than at the filesystem root.
let effectiveLibraryPath = '';

async function browseFetch(path) {
  const url = '/api/admin/browse' + (path ? `?path=${encodeURIComponent(path)}` : '');
  // same-origin credentials carry the session cookie; nothing to attach by hand.
  return api(url, { cache: 'no-store' });
}

// Resolves to {username, password} or null if dismissed. A native prompt() was
// used here first: it cannot be themed, and it has no password mode, so the
// password appeared in clear on screen.
function askCredentials() {
  const modal = document.getElementById('signin-modal');
  const user = document.getElementById('signin-user');
  const pass = document.getElementById('signin-pass');
  const err = document.getElementById('signin-error');
  const submit = document.getElementById('signin-submit');
  const cancel = document.getElementById('signin-cancel');
  if (!modal) return Promise.resolve(null);

  user.value = '';
  pass.value = '';
  err.textContent = '';
  // Keep focus and assistive tech inside the dialog: the page behind it is inert while it is open.
  const opener = document.activeElement;
  const app = document.querySelector('.app');
  if (app) app.inert = true;
  modal.hidden = false;
  // Focus straight away rather than inside requestAnimationFrame: rAF only runs
  // when the page is producing frames, so a background or non-compositing tab
  // would open the dialog with nothing focused. setTimeout is the belt-and-braces
  // retry for browsers that will not focus an element in the same tick it is shown.
  user.focus();
  if (document.activeElement !== user) setTimeout(() => user.focus(), 0);

  return new Promise(resolve => {
    const close = (value) => {
      modal.hidden = true;
      if (app) app.inert = false;
      if (opener && typeof opener.focus === 'function') opener.focus();
      submit.removeEventListener('click', onSubmit);
      cancel.removeEventListener('click', onCancel);
      modal.removeEventListener('keydown', onKey);
      modal.removeEventListener('mousedown', onBackdrop);
      pass.value = '';
      resolve(value);
    };
    const onSubmit = () => {
      if (!user.value.trim() || !pass.value) {
        err.textContent = 'Both a username and a password are required.';
        return;
      }
      close({ username: user.value.trim(), password: pass.value });
    };
    const onCancel = () => close(null);
    const onKey = (e) => {
      if (e.key === 'Escape') { e.preventDefault(); onCancel(); return; }
      // Enter submits from a field or the Sign in button. On Cancel it cancels, like a click.
      if (e.key === 'Enter' && (e.target.tagName === 'INPUT' || e.target === submit)) {
        e.preventDefault();
        onSubmit();
        return;
      }
      if (e.key === 'Tab') {
        const stops = Array.from(modal.querySelectorAll('input, button')).filter(el => !el.disabled);
        const first = stops[0];
        const last = stops[stops.length - 1];
        if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
        else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
      }
    };
    // Clicking the backdrop dismisses, but only the backdrop itself: a drag
    // that starts inside the card must not count as an outside click.
    const onBackdrop = (e) => { if (e.target === modal) onCancel(); };

    submit.addEventListener('click', onSubmit);
    cancel.addEventListener('click', onCancel);
    modal.addEventListener('keydown', onKey);
    modal.addEventListener('mousedown', onBackdrop);
  });
}

// The browser's own confirm(), alert() and prompt() boxes ignore the dashboard's look, so every
// question goes through this one dialog: a title, the words, and buttons named for what they do.
// A destructive question opens on Cancel and colours its button red. Resolves true or false, or,
// with a typed answer, the text entered or null. `mustEqual` keeps the button off until it matches.
function askDialog({ title, message = '', confirm = 'OK', cancel = 'Cancel', danger = false, input = null }) {
  const modal = document.getElementById('ask-modal');
  if (!modal) return Promise.resolve(input && !input.readOnly ? null : false);
  const byId = id => document.getElementById(id);
  const ok = byId('ask-ok');
  const no = byId('ask-cancel');
  const field = byId('ask-input');
  const typed = !!input && !input.readOnly;
  byId('ask-title').textContent = title;
  byId('ask-desc').textContent = message;
  ok.textContent = confirm;
  ok.classList.toggle('btn-destructive', danger);
  no.textContent = cancel ?? '';
  no.hidden = cancel === null;
  byId('ask-fields').hidden = !input;
  if (input) {
    byId('ask-label').textContent = input.label ?? '';
    byId('ask-label').hidden = !input.label;
    field.value = input.value ?? '';
    field.placeholder = input.placeholder ?? '';
    field.readOnly = !!input.readOnly;
  }
  const sync = () => { ok.disabled = input?.mustEqual != null && field.value.trim() !== input.mustEqual; };
  sync();

  const opener = document.activeElement;
  const app = document.querySelector('.app');
  if (app) app.inert = true;
  modal.hidden = false;
  const first = input ? field : (danger && cancel !== null ? no : ok);
  first.focus();
  if (document.activeElement !== first) setTimeout(() => first.focus(), 0);
  if (input?.readOnly) field.select();

  return new Promise(resolve => {
    const close = value => {
      modal.hidden = true;
      if (app) app.inert = false;
      if (opener && typeof opener.focus === 'function') opener.focus();
      ok.removeEventListener('click', onOk);
      no.removeEventListener('click', onNo);
      modal.removeEventListener('keydown', onKey);
      modal.removeEventListener('mousedown', onBackdrop);
      field.removeEventListener('input', sync);
      resolve(value);
    };
    const onOk = () => { if (!ok.disabled) close(typed ? field.value.trim() : true); };
    const onNo = () => close(typed ? null : false);
    const onKey = e => {
      if (e.key === 'Escape') { e.preventDefault(); onNo(); return; }
      if (e.key === 'Enter' && (e.target === field || e.target === ok)) { e.preventDefault(); onOk(); return; }
      if (e.key === 'Tab') {
        const stops = Array.from(modal.querySelectorAll('input, button')).filter(el => !el.disabled && !el.hidden && el.offsetParent !== null);
        const head = stops[0];
        const tail = stops[stops.length - 1];
        if (e.shiftKey && document.activeElement === head) { e.preventDefault(); tail.focus(); }
        else if (!e.shiftKey && document.activeElement === tail) { e.preventDefault(); head.focus(); }
      }
    };
    // Only the backdrop itself: a drag that starts inside the card is not an outside click.
    const onBackdrop = e => { if (e.target === modal) onNo(); };
    ok.addEventListener('click', onOk);
    no.addEventListener('click', onNo);
    modal.addEventListener('keydown', onKey);
    modal.addEventListener('mousedown', onBackdrop);
    field.addEventListener('input', sync);
  });
}

const askConfirm = (title, message, confirm, danger = false) => askDialog({ title, message, confirm, danger });

async function browseAuthenticate(result) {
  const creds = await askCredentials();
  if (!creds) {
    // Without this the area that asked kept saying "Loading…" forever.
    result.textContent = 'Sign-in cancelled. This needs a Navidrome admin account.';
    return false;
  }
  const r = await api('/api/admin/browse/auth', {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(creds),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) {
    result.innerHTML = esc(data.error || 'Sign-in failed.');
    return false;
  }
  showSignedIn(data.user);
  return true;   // the session is now in the cookie the response set, kept for this browser
}

// The footer says who this browser is signed in as. The sign-in is remembered across restarts
// and lapses only after 90 days without a visit (an hour for the recovery code); Sign out ends it now.
function showSignedIn(user, recovery = false) {
  const line = document.getElementById('signed-in');
  if (!line) return;
  line.hidden = !user;
  document.getElementById('signed-in-user').textContent =
    !user ? '' : recovery ? 'Signed in with the recovery code' : `Signed in as ${user}`;
  const everywhere = document.getElementById('sign-out-everywhere');
  if (everywhere) everywhere.hidden = !user || recovery;
}

async function loadSignedIn() {
  try {
    const r = await api('/api/admin/browse/session', { credentials: 'same-origin' });
    const body = await r.json();
    showSignedIn(body.signedIn ? body.user : null, body.recovery);
  } catch { showSignedIn(null); }
}

document.getElementById('sign-out')?.addEventListener('click', async () => {
  await api('/api/admin/browse/signout', { method: 'POST', credentials: 'same-origin' });
  // Back to the sign-in screen, or to the dashboard when the sign-in is off.
  location.reload();
});

document.getElementById('sign-out-everywhere')?.addEventListener('click', async () => {
  if (!await askConfirm('Sign out everywhere?', 'Every browser and script signed in as you has to sign in again.', 'Sign out everywhere', true)) return;
  await api('/api/admin/browse/signout?everywhere=true', { method: 'POST' });
  location.reload();
});

function renderBrowse(data, result, input) {
  const rows = [];
  if (data.parent) {
    rows.push(`<button type="button" class="btn btn-ghost detect-pick browse-nav" data-path="${esc(data.parent)}">.. <span class="detect-tag">up</span></button>`);
  }
  for (const e of data.entries || []) {
    rows.push(`<button type="button" class="btn btn-ghost detect-pick browse-nav" data-path="${esc(e.path)}">${esc(e.name)}<span class="detect-tag">${e.writable ? 'writable' : 'read-only'}</span></button>`);
  }
  // The track count is what tells you this is the right folder. A library of
  // loose files under a few album folders otherwise renders as almost empty,
  // because only directories are listed.
  const tracks = data.audioFiles > 0
    ? ` <span class="detect-tag">${data.audioFiles.toLocaleString()} audio ${data.audioFiles === 1 ? 'file' : 'files'} here</span>`
    : (data.path && !data.entries?.length ? ' <span class="detect-tag">empty</span>' : '');
  const here = data.path
    ? `<code>${esc(data.path)}</code>${tracks} ${data.writable ? '' : '<strong>(Octo cannot write here)</strong>'}`
    : 'Drives';
  const note = data.containerised
    ? '<div class="detect-tag">Octo runs in a container, so this is what it can see. The host\'s own drives show only if they are mounted.</div>'
    : '';
  const useBtn = data.path && data.exists
    ? `<button type="button" class="btn" id="browse-use" data-path="${esc(data.path)}">Use this folder</button>`
    : '';
  const truncated = data.truncated ? '<div class="detect-tag">Showing the first 1000 folders.</div>' : '';

  result.innerHTML = `${here}${note}<div class="detect-list">${rows.join('')}</div>${truncated}<div style="margin-top:8px">${useBtn}</div>`;

  result.querySelectorAll('.browse-nav').forEach(b =>
    b.addEventListener('click', () => openBrowse(b.dataset.path)));
  document.getElementById('browse-use')?.addEventListener('click', () => {
    input.value = data.path;
    input.dispatchEvent(new Event('input', { bubbles: true }));
    result.innerHTML = `Set to <code>${esc(data.path)}</code>. Save, then restart Octo to apply.`;
  });
}

async function openBrowse(path) {
  const result = document.getElementById('browse-result');
  const input = document.getElementById('f-download-path');
  if (!result || !input) return;
  result.hidden = false;
  result.textContent = 'Loading…';
  try {
    let r = await browseFetch(path);
    if (r.status === 401) {
      if (!await browseAuthenticate(result)) return;
      r = await browseFetch(path);
    }
    if (!r.ok) {
      const data = await r.json().catch(() => ({}));
      result.innerHTML = esc(data.error || `Browse failed (HTTP ${r.status}).`);
      return;
    }
    renderBrowse(await r.json(), result, input);
  } catch (e) {
    result.textContent = 'Browse failed: ' + (e?.message || 'unknown error');
  }
}

document.getElementById('btn-browse-path')?.addEventListener('click', () => {
  const current = document.getElementById('f-download-path')?.value?.trim();
  openBrowse(current || effectiveLibraryPath || '');
});

// ────────────────────────────────────────────────────────────────
// Detect Subsonic/Navidrome server on the local network
// ────────────────────────────────────────────────────────────────
const detectBtn = document.getElementById('btn-detect-server');
detectBtn?.addEventListener('click', async () => {
  const urlInput = document.getElementById('f-subsonic-url');
  const result = document.getElementById('detect-server-result');
  detectBtn.disabled = true;
  const label = detectBtn.querySelector('span') ?? detectBtn;
  const original = label.textContent;
  label.textContent = 'Scanning…';
  if (result) { result.hidden = false; result.textContent = 'Scanning the local network…'; }
  try {
    const r = await api('/api/admin/discover-servers', { cache: 'no-store' });
    const data = await r.json();
    const servers = data.servers || [];
    if (!servers.length) {
      if (result) result.innerHTML =
        'No server found on this network. On a Docker bridge network Octo cannot see your LAN, so use host networking or type the URL in.';
    } else if (servers.length === 1) {
      urlInput.value = servers[0].url;
      urlInput.dispatchEvent(new Event('input', { bubbles: true }));
      if (result) result.innerHTML =
        `Found <strong>${esc(servers[0].type || 'Subsonic')}</strong> ${esc(servers[0].serverVersion || '')} at <code>${esc(servers[0].url)}</code>, filled in above. Save to apply.`;
    } else {
      const rows = servers.map(s =>
        `<button type="button" class="btn btn-ghost detect-pick" data-url="${esc(s.url)}">${esc(s.url)} <span class="detect-tag">${esc(s.type || 'subsonic')} ${esc(s.serverVersion || '')}</span></button>`).join('');
      if (result) result.innerHTML = `Found ${servers.length} servers. Pick one:<div class="detect-list">${rows}</div>`;
      result.querySelectorAll('.detect-pick').forEach(b =>
        b.addEventListener('click', () => {
          urlInput.value = b.dataset.url;
          urlInput.dispatchEvent(new Event('input', { bubbles: true }));
          result.querySelectorAll('.detect-pick').forEach(other => other.classList.toggle('is-picked', other === b));
          note(result.querySelector('.detect-list'), 'Filled in above. Save to apply.');
        }));
    }
  } catch (e) {
    if (result) result.textContent = 'Scan failed: ' + (e?.message || 'unknown error');
  } finally {
    detectBtn.disabled = false;
    label.textContent = original;
  }
});

// ────────────────────────────────────────────────────────────────
// Fetched songs: the running download log
// ────────────────────────────────────────────────────────────────
function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, c =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
function relTime(iso) {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 604800) return `${Math.floor(s / 86400)}d ago`;
  return new Date(t).toLocaleDateString();
}
function fmtSize(bytes) {
  if (!bytes) return '';
  const mb = bytes / 1048576;
  return mb >= 1 ? `${mb.toFixed(1)} MB` : `${Math.round(bytes / 1024)} KB`;
}
// Hearted downloads still running, or failed in the last half hour. Finished ones are left to
// the log below, which gets them the moment they land. Polled while the pane is open and
// something is still moving, and not otherwise.
const ACQ_LABELS = {
  queued: 'Queued', searching: 'Searching', downloading: 'Downloading',
  verifying: 'Checking', importing: 'Adding to library', failed: 'Failed',
};
let acqTimer = null;
let acqMoving = new Set();
async function loadAcquisitions() {
  const wrap = document.getElementById('acq-wrap');
  const list = document.getElementById('acq-list');
  if (!wrap || !list) return;
  clearTimeout(acqTimer);
  let rows = [];
  try {
    const r = await api('/api/admin/acquisitions', { cache: 'no-store' });
    if (r.ok) rows = ((await r.json()).acquisitions || []).filter(a => a.state !== 'done');
  } catch { /* the log below still loads */ }

  const moving = new Set(rows.filter(a => a.state !== 'failed').map(a => a.id));
  // Something that was moving and no longer is has most likely just landed in the log.
  const landed = [...acqMoving].some(id => !moving.has(id));
  acqMoving = moving;

  wrap.hidden = rows.length === 0;
  const focused = dlogFocusKey(list);
  list.innerHTML = rows.map(a => {
    const pct = typeof a.progress === 'number' ? Math.round(a.progress * 100) : null;
    const failed = a.state === 'failed';
    const label = a.state === 'downloading' && pct !== null
      ? `Downloading ${pct}%`
      : a.state === 'queued' && a.ahead > 0
        ? `Queued, ${a.ahead} ahead`
        : (ACQ_LABELS[a.state] || a.state);
    const askers = Array.isArray(a.requestedBy) ? a.requestedBy.filter(Boolean) : [];
    const size = fmtSize(a.bytesTotal);
    const sub = [
      a.artist ? escapeHtml(a.artist) : '',
      a.source ? escapeHtml(a.source) : '',
      askers.length ? `asked by <span class="dl-asker">${escapeHtml(askers.join(', '))}</span>` : '',
      escapeHtml(relTime(a.startedAt)),
      size,
    ].filter(Boolean).join(' · ');
    const detail = failed && a.error
      ? `<div class="acq-error">${escapeHtml(a.error)}</div>`
      : pct !== null && a.state === 'downloading'
        ? `<div class="acq-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${pct}"><span style="width:${pct}%"></span></div>`
        : a.note
          ? `<div class="dl-sub">${escapeHtml(a.note)}</div>`
          : '';
    // No cover yet for a song still on its way: its state's glyph on a tile instead.
    const glyph = failed ? 'i-warning' : a.state === 'queued' ? 'i-clock' : 'i-tray-arrow-down';
    return `<div class="acq-card${failed ? ' failed' : ''}${dlogIsOpen(a.key) ? ' has-log' : ''}">
      <div class="acq-art"><svg class="icon" aria-hidden="true"><use href="#${glyph}"/></svg></div>
      <div class="dl-main">
        <div class="acq-head">
          <div class="dl-title">${escapeHtml(a.title || '?')}</div>
          <span class="dl-badge ${failed ? 'failed' : 'state'}">${escapeHtml(label)}</span>
        </div>
        <div class="dl-sub">${sub}</div>
        ${detail}
        ${acqActions(a)}
      </div>
      ${acqLog(a)}
    </div>`;
  }).join('');
  dlogAfterRender(list, focused);

  const open = document.querySelector('section[data-pane="fetched"]')?.classList.contains('active');
  if (moving.size && open) acqTimer = setTimeout(loadAcquisitions, 3000);
  if (landed) loadFetched({ withAcquisitions: false });
}

async function loadFetched({ withAcquisitions = true } = {}) {
  if (withAcquisitions) loadAcquisitions();
  const list = document.getElementById('fetched-list');
  if (!list) return;
  try {
    const r = await api('/api/admin/downloads', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();
    const items = data.downloads || [];
    if (!items.length) {
      list.innerHTML = stateBlock('empty', 'Nothing fetched yet. Heart a song Octo found for you in your music app, and it shows up here once it is in your library.');
      return;
    }
    list.innerHTML = items.map(d => {
      const fmt = (d.format || '?').toUpperCase();
      const badgeClass = fmt === 'FLAC' ? 'flac' : 'mp3';
      const art = d.coverArtUrl
        ? `<img class="dl-art" src="${escapeHtml(d.coverArtUrl)}" alt="" loading="lazy" onerror="this.replaceWith(Object.assign(document.createElement('div'),{className:'dl-art dl-art-ph'}))">`
        : `<div class="dl-art dl-art-ph"></div>`;
      const size = fmtSize(d.sizeBytes);
      // Absent on entries written before attribution existed, and whenever the setting
      // is off, so the row has to read the same with and without it.
      const askers = Array.isArray(d.requestedBy) ? d.requestedBy.filter(Boolean) : [];
      const who = askers.length
        ? `<span class="dl-asker">${escapeHtml(askers.join(', '))}</span>`
        : '';
      const by = [d.artist, d.album].filter(Boolean).map(escapeHtml).join(' <span class="dl-dash">·</span> ');
      return `<div class="dl-item">
        ${art}
        <div class="dl-main">
          <div class="dl-title">${escapeHtml(d.title)}</div>
          <div class="dl-by">${by}</div>
          <div class="dl-path" title="${escapeHtml(d.path)}"><bdi>${escapeHtml(d.path)}</bdi></div>
        </div>
        <div class="dl-side">
          <div class="dl-tags"><span class="dl-badge ${badgeClass}">${escapeHtml(fmt)}</span><span class="dl-source">${escapeHtml(d.source)}</span>${fetchedFindButton(d)}</div>
          <div class="dl-sub">${escapeHtml(relTime(d.downloadedAt))}${size ? ' · ' + size : ''}${who ? ' · ' + who : ''}</div>
        </div>
      </div>${d.tagging ? `<details class="dl-tagging"><summary>How it was tagged</summary>${renderTagReport(d.tagging)}</details>` : ''}${fetchedLog(d)}`;
    }).join('');
  } catch (e) {
    list.innerHTML = stateBlock('error', `Couldn't load the log: ${e.message || 'error'}`);
  }
}
document.getElementById('fetched-refresh')?.addEventListener('click', loadFetched);

// ── Afterglow: the page takes its light from a fetched song's cover ─────────────────────────
// Octo reads each cover with the station covers' colour rules (GET /api/admin/ambient): three
// lights behind the glass and an accent. The song chosen under Recently added is remembered in
// this browser; with none chosen, the newest song lights the page. A cover with no colour at
// all leaves Octo's own blue-greys. The last palette is kept too, so a reload starts in it.
const AMBIENT_PICK_KEY = 'octo.ambient.pick';
const AMBIENT_LAST_KEY = 'octo.ambient.last';
let ambientSongs = [];

function ambientStoreGet(key) { try { return localStorage.getItem(key); } catch { return null; } }
function ambientStoreSet(key, value) {
  try { if (value == null) localStorage.removeItem(key); else localStorage.setItem(key, value); } catch { /* storage off */ }
}
const ambientHex = v => typeof v === 'string' && /^#[0-9a-f]{6}$/i.test(v);
const ambientKey = s => `${s.downloadedAt}|${s.artist}|${s.title}`;

function applyAmbient(colors, accent) {
  const style = document.documentElement.style;
  if (!Array.isArray(colors) || colors.length < 3 || !colors.every(ambientHex) || !ambientHex(accent)) {
    ['--amb-1', '--amb-2', '--amb-3', '--accent-rgb'].forEach(p => style.removeProperty(p));
    return false;
  }
  colors.slice(0, 3).forEach((c, i) => style.setProperty(`--amb-${i + 1}`, c));
  const n = parseInt(accent.slice(1), 16);
  style.setProperty('--accent-rgb', `${(n >> 16) & 255} ${(n >> 8) & 255} ${n & 255}`);
  return true;
}

// Start in the last colour before anything has loaded.
try {
  const last = JSON.parse(ambientStoreGet(AMBIENT_LAST_KEY) || 'null');
  if (last) applyAmbient(last.colors, last.accent);
} catch { /* nothing kept */ }

function chosenAmbientSong() {
  const picked = ambientStoreGet(AMBIENT_PICK_KEY);
  // The chosen song, else the newest: never some other song that happens to have colour, or
  // choosing the newest would show a different one.
  return ambientSongs.find(s => ambientKey(s) === picked) || ambientSongs[0] || null;
}

function renderLatestFetched() {
  const card = document.getElementById('latest-fetched');
  if (!card) return;
  const song = chosenAmbientSong();
  if (applyAmbient(song?.colors, song?.accent)) {
    ambientStoreSet(AMBIENT_LAST_KEY, JSON.stringify({ colors: song.colors, accent: song.accent }));
  } else {
    ambientStoreSet(AMBIENT_LAST_KEY, null);
  }
  card.hidden = !song;
  if (!song) return;

  const art = document.getElementById('latest-art');
  if (song.coverArtUrl) {
    art.hidden = false;
    art.src = song.coverArtUrl;
    art.onerror = () => { art.hidden = true; };
  } else {
    art.hidden = true;
    art.removeAttribute('src');
  }
  document.getElementById('latest-kicker').textContent = song === ambientSongs[0] ? 'Latest fetched' : 'Lighting the page';
  document.getElementById('latest-title').textContent = song.title;
  document.getElementById('latest-sub').textContent = [song.artist, song.album].filter(Boolean).join(' · ');
  const chips = [
    `<span class="now-chip format">${escapeHtml((song.format || '?').toUpperCase())}</span>`,
    song.source ? `<span class="now-chip">${escapeHtml(song.source)}</span>` : '',
    fmtSize(song.sizeBytes) ? `<span class="now-chip">${escapeHtml(fmtSize(song.sizeBytes))}</span>` : '',
    relTime(song.downloadedAt) ? `<span class="now-chip">${escapeHtml(relTime(song.downloadedAt))}</span>` : '',
  ];
  document.getElementById('latest-chips').innerHTML = chips.join('');

  const covers = document.getElementById('latest-covers');
  covers.innerHTML = ambientSongs.map((s, i) => {
    const on = s === song;
    const label = `Light the page with ${s.title} by ${s.artist}${s.colors ? '' : ' (no colour in this cover)'}`;
    const img = s.coverArtUrl
      ? `<img src="${escapeHtml(s.coverArtUrl)}" alt="" loading="lazy" onerror="this.remove()">`
      : '';
    return `<button type="button" class="now-cover" data-ambient="${i}" aria-pressed="${on}" aria-label="${escapeHtml(label)}" title="${escapeHtml(s.artist + ' · ' + s.title)}">${img}</button>`;
  }).join('');
}

document.getElementById('latest-covers')?.addEventListener('click', e => {
  const button = e.target.closest('[data-ambient]');
  if (!button) return;
  const song = ambientSongs[Number(button.dataset.ambient)];
  if (!song) return;
  // Choosing the newest song means "follow the newest", so a song fetched later takes over.
  ambientStoreSet(AMBIENT_PICK_KEY, song === ambientSongs[0] ? null : ambientKey(song));
  renderLatestFetched();
  document.querySelector(`#latest-covers [data-ambient="${button.dataset.ambient}"]`)?.focus();
});

let ambientLoading = null;
function loadAmbient() {
  ambientLoading ??= (async () => {
    try {
      const r = await api('/api/admin/ambient', { cache: 'no-store' });
      if (!r.ok) return;
      const data = await r.json();
      ambientSongs = Array.isArray(data.songs) ? data.songs : [];
      renderLatestFetched();
    } catch {
      // The light is a nicety: the page keeps whatever colour it has.
    } finally {
      ambientLoading = null;
    }
  })();
  return ambientLoading;
}

// ── How a download was tagged ───────────────────────────────────────────────
// The same block under a Fetched songs row and under "Try it on a song": the release that won
// and how sure Octo is, every candidate it weighed with its biggest penalties, what each field
// was set to and from, the notes, the stage timings and the loudness.
function renderTagReport(report) {
  if (!report) return '';
  const confidence = String(report.confidence || 'None');
  const badge = { Strong: 'good', Medium: 'state', Ambiguous: 'warn', Low: 'warn', None: 'mp3' }[confidence] || 'mp3';
  const distance = typeof report.distance === 'number' ? ` · distance ${report.distance.toFixed(3)}` : '';
  const chosen = report.releaseTitle
    ? `<strong>${escapeHtml(report.releaseTitle)}</strong>${report.releaseDate ? ' (' + escapeHtml(report.releaseDate) + ')' : ''} from ${escapeHtml(report.source || '?')}`
    : 'No release was chosen; the tags came from the catalog and the file as before.';
  const rehearsed = report.rehearsed ? '<span class="dl-badge warn">rehearsal</span>' : '';

  const candidates = (report.candidates || []).map(c => `<div class="config-row tag-candidate">
      <div><span class="key">${escapeHtml(c.source)}</span> ${escapeHtml(c.title)}<div class="tag-sub">${escapeHtml(c.album || '')}${c.type ? ' · ' + escapeHtml(c.type) : ''}${c.date ? ' · ' + escapeHtml(c.date) : ''}</div></div>
      <div class="value">${Number(c.distance).toFixed(3)}<div class="tag-sub">${escapeHtml((c.biggestPenalties || []).join(', ') || 'nothing against it')}</div></div>
    </div>`).join('');

  const fields = Object.entries(report.fields || {}).map(([name, f]) => `<div class="config-row">
      <div class="key">${escapeHtml(name)}</div>
      <div class="value">${escapeHtml(f?.value ?? '')}<span class="tag-sub"> from ${escapeHtml(f?.source || '?')}</span></div>
    </div>`).join('');

  const notes = (report.notes || []).map(n => `<li>${escapeHtml(n)}</li>`).join('');
  const stages = Object.entries(report.stageSeconds || {}).map(([k, v]) => `${escapeHtml(k)} ${Number(v).toFixed(1)}s`).join(' · ');
  const loudness = typeof report.integratedLufs === 'number'
    ? `${report.integratedLufs.toFixed(1)} LUFS, peak ${Number(report.truePeakDbfs ?? 0).toFixed(1)} dBTP`
    : 'not measured';

  return `<div class="tag-report">
    <div class="tag-report-head"><span class="dl-badge ${badge}">${escapeHtml(confidence)}</span>${rehearsed}<span>${chosen}${distance}</span></div>
    ${candidates ? `<div class="tag-report-title">Candidates</div><div class="config-table">${candidates}</div>` : ''}
    ${fields ? `<div class="tag-report-title">Fields</div><div class="config-table">${fields}</div>` : ''}
    ${notes ? `<ul class="tag-report-notes">${notes}</ul>` : ''}
    <div class="tag-sub">${stages ? stages + ' · ' : ''}loudness ${escapeHtml(loudness)}${report.detailsPrefetchHit ? ' · release details prefetched' : ''}</div>
  </div>`;
}

// "Try it on a song": the whole identification on a library file or on a name, never written.
document.getElementById('tags-preview-run')?.addEventListener('click', async () => {
  const button = document.getElementById('tags-preview-run');
  const status = document.getElementById('tags-preview-status');
  const out = document.getElementById('tags-preview-out');
  const body = {
    path: document.getElementById('tags-preview-path')?.value.trim() || null,
    artist: document.getElementById('tags-preview-artist')?.value.trim() || null,
    title: document.getElementById('tags-preview-title')?.value.trim() || null,
    album: document.getElementById('tags-preview-album')?.value.trim() || null,
  };
  if (!body.path && !(body.artist && body.title)) {
    status.textContent = 'Give a path inside the music folder, or an artist and a title.';
    status.hidden = false;
    return;
  }
  button.disabled = true;
  status.textContent = 'Asking the fingerprint service, the catalog and the music database…';
  status.hidden = false;
  out.hidden = true;
  try {
    const r = await genreBackfillFetch('/api/admin/tags/preview', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const result = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
    out.innerHTML = renderTagReport(result);
    out.hidden = false;
    status.hidden = true;
  } catch (e) {
    status.textContent = `Could not try it: ${e.message || 'error'}`;
  } finally {
    button.disabled = false;
  }
});

// ────────────────────────────────────────────────────────────────
// Download logs, Find songs and Recently removed: what the Octo apps show, for every admin
// ────────────────────────────────────────────────────────────────
// Every call here needs a Navidrome admin's own sign-in (the recovery code is not enough: these
// name peers and files), so a refusal opens the same Navidrome sign-in the Better quality page uses.
async function parityFetch(path, options = {}, holder = null) {
  let response = await api(path, { credentials: 'same-origin', cache: 'no-store', ...options });
  if (response.status === 401 && await browseAuthenticate(holder)) {
    response = await api(path, { credentials: 'same-origin', cache: 'no-store', ...options });
  }
  return response;
}
const parityJson = body => ({ method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
async function parityError(response) {
  const body = await response.json().catch(() => ({}));
  return body.error || body.title || `Octo answered ${response.status}.`;
}

// "3:07" from seconds.
function parityLength(seconds) {
  if (typeof seconds !== 'number' || seconds <= 0) return '';
  const s = Math.round(seconds);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}
const parityLossless = format => ['flac', 'wav', 'alac', 'ape', 'aiff', 'aif', 'wv'].includes(String(format || '').toLowerCase().replace(/^\./, '').split(/[ -]/)[0]);
const parityFileName = path => String(path || '').split(/[\\/]/).filter(Boolean).pop() || '';

// ── A download's log ────────────────────────────────────────────────────────
// The same timeline the apps' downloads drawer draws: the searches, the copies found (best
// first, with why each was passed over), the one tried and why, the checks, the tags, the cover,
// the lyrics, the library and the end. In progress cards open it in place; Fetched songs rows
// fold it under the row like "How it was tagged". Saved logs outlive the live list's 3 hours.
const DLOG_GLYPH = {
  queued: 'i-clock', search: 'i-magnifying-glass', found: 'i-stack', try: 'i-arrow-square-out',
  transfer: 'i-tray-arrow-down', check: 'i-check-circle', tags: 'i-tag', cover: 'i-image-square',
  lyrics: 'i-microphone-stage', library: 'i-folder-open', done: 'i-check-circle-fill',
  failed: 'i-x-circle-fill', note: 'i-info',
};
const dlog = { open: new Set(), cache: new Map(), loading: new Set() };
const dlogIsOpen = key => !!key && dlog.open.has(key);

function dlogClock(iso) {
  const t = Date.parse(iso);
  return Number.isNaN(t) ? '' : new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

// One copy on a log line: its quality, size, length and peer, and Octo's verdict on it.
function dlogCopy(c) {
  const facts = [c.quality || (c.format || '').toUpperCase(), fmtSize(c.size), parityLength(c.length),
    c.peer ? `from ${c.peer}` : '', typeof c.queueLength === 'number' && c.queueLength > 0 ? `${c.queueLength} in their queue` : '',
    c.freeSlot === true ? 'free slot' : ''].filter(Boolean);
  const verdict = typeof c.rank === 'number'
    ? `<span class="dl-badge good">${c.rank === 1 ? "Octo's first choice" : `Choice ${c.rank}`}</span>`
    : '';
  return `<div class="dlog-copy">
    <div class="dlog-copy-head">${verdict}<span class="dlog-copy-facts">${escapeHtml(facts.join(' · '))}</span></div>
    ${c.file ? `<div class="dl-path" title="${escapeHtml(c.file)}"><bdi>${escapeHtml(c.file)}</bdi></div>` : ''}
    ${c.note ? `<div class="dlog-copy-note">${escapeHtml(c.note)}</div>` : ''}
  </div>`;
}

function renderDlog(events) {
  if (!events?.length) return stateBlock('empty', 'Nothing logged yet.');
  return `<ol class="dlog">${events.map(line => {
    const kind = DLOG_GLYPH[line.kind] ? line.kind : 'note';
    const copies = Array.isArray(line.candidate) ? line.candidate : [];
    const many = copies.length > 3;
    const list = copies.length
      ? (many
        ? `<details class="dlog-more"><summary>${copies.length} copies</summary><div class="dlog-copies">${copies.map(dlogCopy).join('')}</div></details>`
        : `<div class="dlog-copies">${copies.map(dlogCopy).join('')}</div>`)
      : '';
    return `<li class="dlog-line k-${kind}">
      <span class="dlog-dot">${icon(DLOG_GLYPH[kind])}</span>
      <div class="dlog-body">
        <div class="dlog-head"><span class="dlog-text">${escapeHtml(line.text)}</span><time class="dlog-time" datetime="${escapeHtml(line.at)}">${escapeHtml(dlogClock(line.at))}</time></div>
        ${line.detail ? `<div class="dlog-detail">${escapeHtml(line.detail)}</div>` : ''}
        ${list}
      </div>
    </li>`;
  }).join('')}</ol>`;
}

const DLOG_NONE = 'Octo did not keep a log for this download. It saves each download\'s log from this version on, and lets the oldest ones go once they fill their room.';

// What a log answer draws, cached so a list that redraws every few seconds keeps it in place.
function dlogBody(entry) {
  if (!entry) return stateBlock('loading', 'Reading the log…');
  if (entry.error) return stateBlock('error', entry.error);
  if (entry.kept === 'none') return stateBlock('empty', DLOG_NONE);
  return renderDlog(entry.events);
}

async function dlogLoad(cacheKey, key, at, holder) {
  if (!key) { dlog.cache.set(cacheKey, { kept: 'none', events: [] }); if (holder) holder.innerHTML = dlogBody(dlog.cache.get(cacheKey)); return; }
  if (dlog.loading.has(cacheKey)) return;
  dlog.loading.add(cacheKey);
  try {
    const query = new URLSearchParams({ key });
    if (at) query.set('at', at);
    const response = await parityFetch(`/api/admin/downloads/log?${query}`, {}, holder);
    if (!response.ok) throw new Error(await parityError(response));
    const body = await response.json();
    dlog.cache.set(cacheKey, { kept: body.kept, events: body.event || [] });
  } catch (error) {
    dlog.cache.set(cacheKey, { error: `Could not read the log: ${error.message}` });
  } finally {
    dlog.loading.delete(cacheKey);
  }
  // The row may have been drawn again meanwhile; fill whichever holder is on the page now.
  document.querySelectorAll('[data-dlog-holder]').forEach(el => {
    if (el.dataset.dlogHolder === cacheKey) el.innerHTML = dlogBody(dlog.cache.get(cacheKey));
  });
}

// In progress: Log and Find songs under each card, and the log across the panel when open.
function acqActions(a) {
  if (!a.key) return '';
  const open = dlogIsOpen(a.key);
  return `<div class="acq-actions">
    <button type="button" class="btn btn-ghost btn-sm" data-dlog-toggle="${escapeHtml(a.key)}" aria-expanded="${open}" data-focus-id="log:${escapeHtml(a.key)}">${icon(open ? 'i-caret-down' : 'i-caret-right')}<span>Log${a.logLines ? ` (${a.logLines})` : ''}</span></button>
    <button type="button" class="btn btn-ghost btn-sm" data-find-id="${escapeHtml(a.key)}" data-find-title="${escapeHtml(a.title || '')}" data-find-artist="${escapeHtml(a.artist || '')}" data-focus-id="find:${escapeHtml(a.key)}">${icon('i-magnifying-glass')}<span>Find songs</span></button>
  </div>`;
}
function acqLog(a) {
  if (!dlogIsOpen(a.key)) return '';
  return `<div class="acq-log" data-dlog-holder="${escapeHtml(a.key)}">${dlogBody(dlog.cache.get(a.key))}</div>`;
}
// Which control had focus, so a redraw every few seconds never throws a keyboard user out.
function dlogFocusKey(list) {
  const active = document.activeElement;
  return active && list.contains(active) ? active.dataset.focusId || null : null;
}
function dlogAfterRender(list, focused) {
  // A running download's log keeps growing, so an open one is read again with each redraw.
  for (const key of dlog.open) if (list.querySelector(`[data-dlog-holder="${CSS.escape(key)}"]`)) dlogLoad(key, key, null, null);
  if (focused) list.querySelector(`[data-focus-id="${CSS.escape(focused)}"]`)?.focus();
}
document.getElementById('acq-list')?.addEventListener('click', event => {
  const toggle = event.target.closest('[data-dlog-toggle]');
  if (!toggle) return;
  const key = toggle.dataset.dlogToggle;
  const card = toggle.closest('.acq-card');
  if (dlog.open.has(key)) {
    dlog.open.delete(key);
    card?.classList.remove('has-log');
    card?.querySelector('.acq-log')?.remove();
  } else {
    dlog.open.add(key);
    card?.classList.add('has-log');
    card?.insertAdjacentHTML('beforeend', acqLog({ key }));
    dlogLoad(key, key, null, card?.querySelector('.acq-log'));
  }
  const open = dlog.open.has(key);
  toggle.setAttribute('aria-expanded', String(open));
  toggle.querySelector('use')?.setAttribute('href', `#${open ? 'i-caret-down' : 'i-caret-right'}`);
});

// Fetched songs: the log folds under the row, read when it is first opened.
function fetchedLog(d) {
  const cacheKey = `${d.key || ''}|${d.downloadedAt || ''}`;
  const open = dlog.open.has(cacheKey);
  return `<details class="dl-tagging dl-log" data-dlog-key="${escapeHtml(d.key || '')}" data-dlog-at="${escapeHtml(d.downloadedAt || '')}"${open ? ' open' : ''}>
    <summary>Download log${d.hasLog ? '' : d.key ? '' : ' (not kept)'}</summary>
    <div data-dlog-holder="${escapeHtml(cacheKey)}">${open ? dlogBody(dlog.cache.get(cacheKey)) : ''}</div>
  </details>`;
}
function fetchedFindButton(d) {
  const title = d.title || '';
  // An entry written before keys were kept is found by its name instead.
  const data = d.key
    ? `data-find-id="${escapeHtml(d.key)}"`
    : `data-find-q="${escapeHtml([d.artist, title].filter(Boolean).join(' '))}"`;
  return `<button type="button" class="btn btn-ghost btn-icon btn-sm dl-find" ${data} data-find-title="${escapeHtml(title)}" data-find-artist="${escapeHtml(d.artist || '')}" data-find-cover="${escapeHtml(d.coverArtUrl || '')}" aria-label="Find songs for ${escapeHtml(title)}" title="Find songs">${icon('i-magnifying-glass')}</button>`;
}
document.getElementById('fetched-list')?.addEventListener('toggle', event => {
  const details = event.target;
  if (!details.matches?.('details.dl-log')) return;
  const key = details.dataset.dlogKey;
  const cacheKey = `${key}|${details.dataset.dlogAt}`;
  if (!details.open) { dlog.open.delete(cacheKey); return; }
  dlog.open.add(cacheKey);
  const holder = details.querySelector('[data-dlog-holder]');
  const cached = dlog.cache.get(cacheKey);
  holder.innerHTML = dlogBody(cached);
  if (!cached || cached.error || cached.kept === 'live') dlogLoad(cacheKey, key, details.dataset.dlogAt, holder);
}, true);

// ── Find songs ─────────────────────────────────────────────────────────────
// The apps' Find songs: a song's search run again on the admin's own download sources, every
// copy listed with its details and Octo's verdict, and the one picked fetched through the same
// checks as any download. A library song's pick replaces its file through Better quality, so it
// asks first. Started from the search box, or from a row anywhere a song is listed.
const finder = { search: null, look: null, hint: {}, timer: null, shown: 40, token: 0 };
const FIND_PAGE = 40;
const findEl = id => document.getElementById(id);
const findCover = (song, hint) => hint?.cover || (song?.coverArt ? `/api/admin/find/cover?id=${encodeURIComponent(song.coverArt)}` : '');
const findArt = (src, cls = 'dl-art') => src
  ? `<img class="${cls}" src="${escapeHtml(src)}" alt="" loading="lazy" onerror="this.replaceWith(Object.assign(document.createElement('div'),{className:'${cls} dl-art-ph'}))">`
  : `<div class="${cls} dl-art-ph"></div>`;

function lossyFindButton(row) {
  if (!row?.id) return '';
  return ` <button type="button" class="btn btn-ghost btn-sm lossy-find" data-find-id="${esc(row.id)}" data-find-title="${esc(row.title || '')}" data-find-artist="${esc(row.artist || '')}">${icon('i-magnifying-glass')}<span>Find songs</span></button>`;
}

// Anywhere a row offers Find songs, it lands here.
document.addEventListener('click', event => {
  const button = event.target.closest('[data-find-id], [data-find-q]');
  if (!button) return;
  event.preventDefault();
  const hint = { title: button.dataset.findTitle || '', artist: button.dataset.findArtist || '', cover: button.dataset.findCover || '' };
  if (button.dataset.findId) findSongs(button.dataset.findId, hint);
  else findSearch(button.dataset.findQ, true);
});

function findShowSection() {
  if (!document.querySelector('section[data-pane="fetched"].active')) openTab('fetched');
  findEl('find-section')?.scrollIntoView({ block: 'start', behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
}

// The search box: songs in the library (the lyrics page's search) and songs outside it (Octo's own search).
async function findSearch(query, reveal = false) {
  const q = String(query || '').trim();
  const box = findEl('find-q');
  if (box && box.value !== q) box.value = q;
  if (reveal) findShowSection();
  const holder = findEl('find-results');
  if (!holder) return;
  if (!q) { holder.hidden = true; holder.innerHTML = ''; return; }
  holder.hidden = false;
  holder.innerHTML = stateBlock('loading', 'Searching your library and beyond…');
  const token = ++finder.token;
  const [library, outside] = await Promise.all([
    parityFetch(`/api/admin/lyrics/songs?q=${encodeURIComponent(q)}`, {}, holder)
      .then(async r => (r.ok ? { songs: (await r.json()).songs || [] } : { error: await parityError(r) }))
      .catch(error => ({ error: error.message })),
    parityFetch(`/api/admin/find/outside?q=${encodeURIComponent(q)}`, {}, holder)
      .then(async r => (r.ok ? await r.json() : { error: await parityError(r) }))
      .catch(error => ({ error: error.message })),
  ]);
  if (token !== finder.token) return;
  const row = (song, cover, where) => `<div class="dl-item find-hit">
      ${findArt(cover)}
      <div class="dl-main">
        <div class="dl-title">${escapeHtml(song.title || '?')}</div>
        <div class="dl-by">${[song.artist, song.album].filter(Boolean).map(escapeHtml).join(' <span class="dl-dash">·</span> ')}</div>
        <div class="dl-sub">${escapeHtml([where, parityLength(song.duration)].filter(Boolean).join(' · '))}</div>
      </div>
      <div class="dl-side">
        <button type="button" class="btn btn-ghost btn-sm" data-find-id="${escapeHtml(song.id)}" data-find-title="${escapeHtml(song.title || '')}" data-find-artist="${escapeHtml(song.artist || '')}" data-find-cover="${escapeHtml(where === 'In your library' ? '' : (cover || ''))}">${icon('i-magnifying-glass')}<span>Find copies</span></button>
      </div>
    </div>`;
  const group = (title, body) => `<div class="find-group"><h4 class="find-group-title">${escapeHtml(title)}</h4>${body}</div>`;
  const libraryBody = library.error ? stateBlock('error', library.error)
    : library.songs.length ? library.songs.map(song => row(song, `/api/admin/find/cover?id=${encodeURIComponent(song.id)}`, 'In your library')).join('')
      : stateBlock('empty', 'No song in your library matches.');
  const outsideBody = outside.error ? stateBlock('error', outside.error)
    : outside.available === false ? stateBlock('empty', 'Songs outside your library are found with Last.fm, which needs its API key under Last.fm radio.')
      : (outside.songs || []).length ? outside.songs.map(song => row(song, song.coverUrl, 'Not in your library')).join('')
        : stateBlock('empty', 'Nothing outside your library matches.');
  holder.innerHTML = group('In your library', libraryBody) + group('Not in your library', outsideBody);
}
findEl('find-form')?.addEventListener('submit', event => {
  event.preventDefault();
  findSearch(findEl('find-q').value);
});

// A look: start it, then follow it until every source has answered.
async function findSongs(id, hint = {}) {
  const look = findEl('find-look');
  if (!look) return;
  clearTimeout(finder.timer);
  const token = ++finder.token;
  finder.look = null;
  finder.hint = hint;
  finder.shown = FIND_PAGE;
  look.hidden = false;
  look.innerHTML = findTargetHead({ title: hint.title, artist: hint.artist }, hint) + stateBlock('loading', 'Starting the search…');
  findShowSection();
  try {
    const response = await parityFetch('/api/admin/find', parityJson({ id }), look);
    if (!response.ok) throw new Error(await parityError(response));
    if (token !== finder.token) return;
    finder.look = await response.json();
    renderFindLook();
    findFollow(token);
  } catch (error) {
    if (token !== finder.token) return;
    look.innerHTML = findTargetHead({ title: hint.title, artist: hint.artist }, hint) + stateBlock('error', error.message);
  }
}

function findFollow(token) {
  clearTimeout(finder.timer);
  if (finder.look?.state !== 'searching') return;
  finder.timer = setTimeout(async () => {
    if (token !== finder.token) return;
    try {
      const response = await parityFetch(`/api/admin/find/${encodeURIComponent(finder.look.id)}`);
      if (!response.ok) throw new Error(await parityError(response));
      const body = await response.json();
      if (token !== finder.token) return;
      finder.look = body;
      renderFindLook();
    } catch (error) {
      if (token !== finder.token) return;
      finder.look = { ...finder.look, state: 'failed', error: error.message };
      renderFindLook();
      return;
    }
    findFollow(token);
  }, 1500);
}

function findTargetHead(song, hint) {
  const owned = song.libraryId
    ? `In your library as ${[song.quality || (song.format || '').toUpperCase(), fmtSize(song.size)].filter(Boolean).join(', ')}`
    : song.title ? 'Not in your library yet' : '';
  return `<div class="find-target">
    ${findArt(findCover(song, hint))}
    <div class="dl-main">
      <div class="find-target-title">${escapeHtml(song.title || hint.title || 'Finding the song…')}</div>
      <div class="dl-by">${[song.artist || hint.artist, song.album].filter(Boolean).map(escapeHtml).join(' <span class="dl-dash">·</span> ')}${parityLength(song.duration) ? ` <span class="dl-dash">·</span> ${parityLength(song.duration)}` : ''}</div>
      ${owned ? `<div class="dl-sub">${escapeHtml(owned)}</div>` : ''}
    </div>
    <button type="button" class="btn btn-ghost btn-icon" id="find-close" aria-label="Close this search" title="Close">${icon('i-x')}</button>
  </div>`;
}

const FIND_SOURCE_WORDS = { searching: 'Searching', done: 'Done', failed: 'Failed', off: 'Off' };
const FIND_SOURCE_TONE = { searching: 'state', done: 'good', failed: 'failed', off: 'mp3' };

function findCopyRow(c, song, cover, searching) {
  const lossless = parityLossless(c.format);
  const quality = c.bitDepth && c.sampleRate
    ? `${c.bitDepth}-bit ${(c.sampleRate / 1000).toLocaleString(undefined, { maximumFractionDigits: 1 })} kHz`
    : c.bitRate ? `${c.bitRate} kbps` : '';
  const facts = [quality, fmtSize(c.size), parityLength(c.length),
    typeof c.queueLength === 'number' ? (c.queueLength > 0 ? `${c.queueLength} in their queue` : 'no queue') : '',
    c.freeSlot === true ? 'free slot' : c.freeSlot === false ? 'no free slot' : '',
    typeof c.speed === 'number' && c.speed > 0 ? `${fmtSize(c.speed)}/s` : ''].filter(Boolean);
  const title = c.title || parityFileName(c.file) || song.title;
  const ranked = typeof c.rank === 'number';
  const why = ranked
    ? `<div class="find-why good">${escapeHtml(c.rank === 1 ? "Octo's first choice" : `Octo's choice ${c.rank}`)}${c.note ? ` <span class="find-why-more">${escapeHtml(c.note)}</span>` : ''}</div>`
    : c.note ? `<div class="find-why">${escapeHtml(c.note)}</div>` : '';
  const format = (c.format || '').replace(/^\./, '').toUpperCase() || '?';
  return `<div class="dl-item find-copy${ranked && c.rank === 1 ? ' best' : ''}">
    ${findArt(cover)}
    <div class="dl-main">
      <div class="dl-title">${escapeHtml(title)}</div>
      <div class="dl-by">${[c.album || song.album, c.source, c.peer].filter(Boolean).map(escapeHtml).join(' <span class="dl-dash">·</span> ')}</div>
      ${c.file ? `<div class="dl-path" title="${escapeHtml(c.file)}"><bdi>${escapeHtml(c.file)}</bdi></div>` : ''}
      ${why}
    </div>
    <div class="dl-side">
      <div class="dl-tags"><span class="dl-badge ${lossless ? 'flac' : 'mp3'}">${escapeHtml(format)}</span></div>
      ${facts.length ? `<div class="dl-sub">${escapeHtml(facts.join(' · '))}</div>` : ''}
      <button type="button" class="btn btn-sm ${ranked && c.rank === 1 ? 'btn-primary' : 'btn-ghost'}" data-find-pick="${escapeHtml(c.id)}"
        ${searching ? 'disabled title="Pick a copy once the search ends"' : ''}>Get this copy</button>
    </div>
  </div>`;
}

function renderFindLook() {
  const look = findEl('find-look');
  const found = finder.look;
  if (!look || !found) return;
  const song = found.song || {};
  const cover = findCover(song, finder.hint);
  const searching = found.state === 'searching';
  const sources = (found.source || []).map(source => `<div class="find-source">
      <strong>${escapeHtml(source.name)}</strong>
      <span class="dl-badge ${FIND_SOURCE_TONE[source.state] || 'state'}">${escapeHtml(FIND_SOURCE_WORDS[source.state] || source.state)}</span>
      ${source.text ? `<span class="find-source-text">${escapeHtml(source.text)}</span>` : ''}
      ${(source.query || []).length ? `<span class="find-source-text">Asked: ${escapeHtml(source.query.join(', '))}</span>` : ''}
    </div>`).join('');
  const copies = found.candidate || [];
  const shown = copies.slice(0, finder.shown);
  const list = copies.length
    ? `<div class="find-copies">${shown.map(c => findCopyRow(c, song, cover, searching)).join('')}</div>
       ${copies.length > shown.length ? `<button type="button" class="btn btn-ghost" id="find-more">Show ${Math.min(FIND_PAGE, copies.length - shown.length)} more of ${copies.length - shown.length}</button>` : ''}`
    : searching ? stateBlock('loading', 'Asking every peer. This takes up to a minute.') : stateBlock('empty', 'No copies were found.');
  const failed = found.state === 'failed' && found.error ? `<div class="notice notice-warn">${escapeHtml(found.error)}</div>` : '';
  const replaces = song.libraryId && copies.length
    ? `<p class="set-info-d">A copy you pick replaces the one in your library through Better quality: only a lossless copy can, and yours stays until the new one passes every check.</p>` : '';
  look.innerHTML = `${findTargetHead(song, finder.hint)}
    <div class="find-sources">${sources}</div>
    ${failed}${replaces}${list}`;
}

findEl('find-look')?.addEventListener('click', async event => {
  if (event.target.closest('#find-close')) {
    clearTimeout(finder.timer);
    finder.token++;
    finder.look = null;
    findEl('find-look').hidden = true;
    findEl('find-look').innerHTML = '';
    return;
  }
  if (event.target.closest('#find-more')) { finder.shown += FIND_PAGE; renderFindLook(); return; }
  const pick = event.target.closest('[data-find-pick]');
  if (!pick || !finder.look) return;
  const found = finder.look;
  const song = found.song || {};
  const copy = (found.candidate || []).find(c => c.id === pick.dataset.findPick);
  if (song.libraryId) {
    const what = [copy?.quality || (copy?.format || '').toUpperCase(), copy?.peer ? `from ${copy.peer}` : copy?.source].filter(Boolean).join(' ');
    const yours = song.quality || (song.format || '').toUpperCase() || 'your copy';
    if (!(await askConfirm(`Replace your copy of ${song.title}?`,
      `Octo fetches ${what || 'this copy'} and puts it in place of your ${yours} once it passes every check. Your copy waits in the trash until then.`,
      'Replace it'))) return;
  }
  pick.disabled = true;
  try {
    const response = await parityFetch(`/api/admin/find/${encodeURIComponent(found.id)}/pick`, parityJson({ copy: pick.dataset.findPick }), findEl('find-look'));
    if (!response.ok) throw new Error(await parityError(response));
    const outcome = await response.json();
    const queued = outcome.state === 'queued';
    note(pick, song.libraryId && queued ? `${outcome.detail} Follow it on Better quality.` : outcome.detail, queued ? 'ok' : 'info');
    if (queued) loadAcquisitions();
  } catch (error) {
    note(pick, error.message, 'error');
  } finally {
    pick.disabled = false;
  }
});

// ── Recently removed ─────────────────────────────────────────────────────────
// The apps' Put back: the songs still in the server's trash, who removed each and when, how
// long the sweep keeps it, and the lyrics that went with it. Read whenever Library actions opens.
const TRASH_DAY = 24 * 3600 * 1000;
function trashLeft(goneAt) {
  if (!goneAt) return { text: 'Kept until you empty the trash', tone: 'mp3' };
  const days = Math.ceil((Date.parse(goneAt) - Date.now()) / TRASH_DAY);
  if (days <= 1) return { text: 'Deleted for good within a day', tone: 'warn' };
  return { text: `${days} days left`, tone: days <= 3 ? 'warn' : 'state' };
}

async function loadTrash() {
  const list = findEl('trash-list');
  const summary = findEl('trash-summary');
  if (!list) return;
  list.innerHTML = stateBlock('loading', 'Reading the trash…');
  try {
    const response = await parityFetch('/api/admin/trash', {}, list);
    if (!response.ok) throw new Error(await parityError(response));
    const body = await response.json();
    const songs = body.songs || [];
    const keep = body.keepDays > 0 ? `The sweep deletes a song ${body.keepDays} ${body.keepDays === 1 ? 'day' : 'days'} after it was removed.` : 'Nothing is ever deleted from the trash.';
    summary.textContent = `${songs.length} ${songs.length === 1 ? 'song' : 'songs'} in the trash. ${keep}`;
    const rehearsal = body.dryRun ? `<div class="notice notice-warn">Library actions only rehearse while dry run is on, so Put back only says what it would do.</div>` : '';
    if (!songs.length) { list.innerHTML = rehearsal + stateBlock('empty', 'Nothing has been removed lately.'); return; }
    list.innerHTML = rehearsal + songs.map(song => {
      const left = trashLeft(song.goneAt);
      const sidecars = (song.sidecars || []).length ? `with ${song.sidecars.join(', ')}` : '';
      const sub = [`Removed ${relTime(song.removedAt)} by ${song.removedBy || 'someone'}`, sidecars].filter(Boolean).join(' · ');
      return `<div class="dl-item trash-item">
        <div class="acq-art">${icon('i-trash')}</div>
        <div class="dl-main">
          <div class="dl-title">${escapeHtml(song.title || '?')}</div>
          <div class="dl-by">${[song.artist, song.album].filter(Boolean).map(escapeHtml).join(' <span class="dl-dash">·</span> ')}</div>
          <div class="dl-sub">${escapeHtml(sub)}</div>
          ${song.blocksDownloads ? '<div class="dl-sub">Removed from disk, so a heart does not download it again.</div>' : ''}
        </div>
        <div class="dl-side">
          <div class="dl-tags"><span class="dl-badge ${left.tone}">${escapeHtml(left.text)}</span></div>
          <div class="trash-actions">
            ${song.blocksDownloads ? `<button type="button" class="btn btn-ghost btn-sm" data-trash-allow="${escapeHtml(song.key || '')}">Allow downloading again</button>` : ''}
            <button type="button" class="btn btn-primary btn-sm" data-trash-restore="${escapeHtml(song.id)}">${icon('i-arrow-counter-clockwise')}<span>Put back</span></button>
          </div>
        </div>
      </div>`;
    }).join('');
  } catch (error) {
    summary.textContent = 'Needs your Navidrome admin sign-in, and you on the allowed list above.';
    list.innerHTML = `<div class="notice notice-warn">${escapeHtml(error.message)}</div>`;
  }
}

findEl('trash-refresh')?.addEventListener('click', loadTrash);
findEl('trash-list')?.addEventListener('click', async event => {
  const restore = event.target.closest('[data-trash-restore]');
  const allow = event.target.closest('[data-trash-allow]');
  const button = restore || allow;
  if (!button) return;
  button.disabled = true;
  try {
    const response = restore
      ? await parityFetch('/api/admin/trash/restore', parityJson({ id: restore.dataset.trashRestore }), findEl('trash-list'))
      : await parityFetch('/api/admin/library-actions/allow-download', parityJson({ key: allow.dataset.trashAllow }), findEl('trash-list'));
    if (!response.ok) throw new Error(await parityError(response));
    const body = await response.json().catch(() => ({}));
    const done = restore ? body.state === 'applied' : true;
    const words = restore ? body.detail || 'Put back.' : 'It can be downloaded again.';
    if (!done) { note(button, words, body.state === 'rehearsed' ? 'info' : 'error'); button.disabled = false; return; }
    toast(restore ? `${words} Navidrome shows it after its next scan.` : words);
    await loadTrash();
  } catch (error) {
    note(button, error.message, 'error');
    button.disabled = false;
  }
});
// Read each time Library actions opens, without touching the shared tab switcher.
(() => {
  const pane = document.querySelector('section[data-pane="libraryactions"]');
  if (!pane) return;
  let wasActive = pane.classList.contains('active');
  new MutationObserver(() => {
    const active = pane.classList.contains('active');
    if (active && !wasActive) loadTrash();
    wasActive = active;
  }).observe(pane, { attributes: true, attributeFilter: ['class'] });
  if (wasActive) ready.then(loadTrash);
})();

// ────────────────────────────────────────────────────────────────
// Segmented controls: buttons built from a hidden <select> they proxy to,
// so the load/save logic keeps reading the select's name + value.
// ────────────────────────────────────────────────────────────────
function buildSegments() {
  document.querySelectorAll('.seg[data-seg-for]').forEach(seg => {
    if (seg.dataset.built) return;
    const sel = document.getElementById(seg.dataset.segFor);
    if (!sel) return;
    seg.setAttribute('role', 'group');
    const title = seg.closest('.set-row')?.querySelector('.set-info-t');
    if (title?.id) seg.setAttribute('aria-labelledby', title.id);
    else if (sel.getAttribute('aria-label')) seg.setAttribute('aria-label', sel.getAttribute('aria-label'));
    Array.from(sel.options).forEach(opt => {
      const b = document.createElement('button');
      b.type = 'button';
      b.textContent = opt.textContent;
      b.dataset.value = opt.value;
      b.addEventListener('click', () => {
        sel.value = opt.value;
        sel.dispatchEvent(new Event('input', { bubbles: true }));
        sel.dispatchEvent(new Event('change', { bubbles: true }));
        syncSegment(seg, true);
      });
      seg.appendChild(b);
    });
    seg.dataset.built = '1';
  });
}
function syncSegment(seg, animate) {
  const sel = document.getElementById(seg.dataset.segFor);
  if (!sel) return;
  const thumb = seg.querySelector('.seg-thumb');
  let active = null;
  seg.querySelectorAll('button').forEach(b => {
    const on = b.dataset.value === sel.value;
    b.classList.toggle('active', on);
    b.setAttribute('aria-pressed', String(on));
    if (on) active = b;
  });
  if (active && thumb && active.offsetWidth) {
    if (!animate) thumb.style.transition = 'none';
    thumb.style.left = active.offsetLeft + 'px';
    thumb.style.width = active.offsetWidth + 'px';
    thumb.style.opacity = '1';
    if (!animate) requestAnimationFrame(() => { thumb.style.transition = ''; });
  }
}
function syncSegments(animate) {
  document.querySelectorAll('.seg[data-seg-for]').forEach(s => syncSegment(s, animate));
}
decoratePageHeaders();
buildHints();
labelSettingControls();
buildSegments();
window.addEventListener('resize', () => syncSegments(false));

// ────────────────────────────────────────────────────────────────
// "Point your apps at Octo" address + copy
// ────────────────────────────────────────────────────────────────
// The local address is derived, because that one genuinely moves: a DHCP lease
// changes it and nobody memorises a container IP. The remote address is not
// derived, because it cannot be. Octo does see the public hostname on relayed
// Subsonic calls, but only /admin would display it and a sane proxy setup keeps
// /admin off the public internet, so the value never reaches the page that wants
// it. Detecting it would also be answering a question nobody has: whoever set up
// the proxy picked that hostname and has already typed it into a client. So it is
// stored, not discovered, and the row stays hidden until they store it.
function normalizeAddress(raw) {
  const v = (raw || '').trim().replace(/\/+$/, '');
  if (!v) return '';
  return /^https?:\/\//i.test(v) ? v : `https://${v}`;
}

function bindCopy(buttonId, getAddress) {
  document.getElementById(buttonId)?.addEventListener('click', async () => {
    const addr = getAddress();
    if (!addr) return;
    try {
      await navigator.clipboard.writeText(addr);
      const button = document.getElementById(buttonId);
      const label = button?.querySelector('span');
      if (label) {
        label.textContent = 'Copied';
        setTimeout(() => { label.textContent = 'Copy'; }, 1600);
      }
    } catch { /* clipboard blocked; user can select manually */ }
  });
}

function localOctoAddress() {
  const here = new URL(location.href);
  return `${here.protocol}//${here.hostname}:${here.port || '5274'}`;
}

function renderOctoAddresses() {
  const el = document.getElementById('octo-address');
  if (el) el.textContent = localOctoAddress();

  const row = document.getElementById('octo-public-address-row');
  const code = document.getElementById('octo-public-address');
  if (!row || !code) return;
  const publicAddr = normalizeAddress(currentSettings?.Server?.PublicUrl);
  code.textContent = publicAddr;
  row.hidden = !publicAddr;
}

bindCopy('copy-octo-address', localOctoAddress);
bindCopy('copy-octo-public-address',
  () => normalizeAddress(currentSettings?.Server?.PublicUrl));
renderOctoAddresses();

// ────────────────────────────────────────────────────────────────
// Page headers, the folded sidebar, and hints
// ────────────────────────────────────────────────────────────────
// Each page's header carries the icon its sidebar entry has, so the two are always the same
// picture. Copied from the sidebar rather than written twice.
function decoratePageHeaders() {
  navItems.forEach(item => {
    const header = document.querySelector(`section[data-pane="${item.dataset.tab}"] .page-header`);
    const glyph = item.querySelector('svg.icon');
    if (!header || !glyph || header.querySelector('.page-icon')) return;
    const tile = document.createElement('span');
    tile.className = 'page-icon';
    tile.setAttribute('aria-hidden', 'true');
    tile.appendChild(glyph.cloneNode(true));
    header.prepend(tile);
    // A folded sidebar shows icons only, so each keeps its name as a tooltip.
    const label = item.querySelector('span')?.textContent.trim();
    if (label) item.title = label;
  });

  // A long page gets a row of links to its sections under the header, as the desktop app's
  // settings list its sections beside the page.
  document.querySelectorAll('section[data-pane]').forEach(pane => {
    const titles = Array.from(pane.querySelectorAll(':scope > .set-section > .set-head > .set-title'));
    const header = pane.querySelector(':scope > .page-header');
    if (titles.length < 4 || !header || pane.querySelector('.page-toc')) return;
    const toc = document.createElement('nav');
    toc.className = 'page-toc';
    toc.setAttribute('aria-label', 'On this page');
    titles.forEach((title, index) => {
      if (!title.id) title.id = `${pane.dataset.pane}-section-${index + 1}`;
      title.setAttribute('tabindex', '-1');
      const link = document.createElement('a');
      link.href = `#${pane.dataset.pane}`;
      link.textContent = title.textContent.trim();
      link.addEventListener('click', event => {
        event.preventDefault();
        const calm = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
        title.closest('.set-section').scrollIntoView({ behavior: calm ? 'auto' : 'smooth', block: 'start' });
        title.focus({ preventScroll: true });
      });
      toc.appendChild(link);
    });
    header.after(toc);
  });
}

// A description says one thing. Anything more sits in a .set-info-more beside it, folded away
// behind a "More" link, so a page reads as a list of settings rather than a wall of text.
function buildHints() {
  let n = 0;
  document.querySelectorAll('.set-info-more').forEach(more => {
    if (more.dataset.built) return;
    more.dataset.built = '1';
    if (!more.id) more.id = `set-more-${++n}`;
    more.hidden = true;
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'hint-toggle';
    toggle.setAttribute('aria-expanded', 'false');
    toggle.setAttribute('aria-controls', more.id);
    toggle.innerHTML = `<span>More</span>${icon('i-caret-down')}`;
    toggle.addEventListener('click', () => {
      const open = more.hidden;
      more.hidden = !open;
      toggle.setAttribute('aria-expanded', String(open));
      toggle.querySelector('span').textContent = open ? 'Less' : 'More';
    });
    const desc = more.previousElementSibling?.classList.contains('set-info-d') ? more.previousElementSibling : null;
    if (desc) desc.append(' ', toggle);
    else more.before(toggle);
  });
}

// ────────────────────────────────────────────────────────────────
// Accessible names and restart chips
// ────────────────────────────────────────────────────────────────
// Most rows describe their control in a sibling .set-info rather than a <label>, which left
// switches and inputs with no accessible name. Wire each one to its row's own title and text,
// and mark every restart-only setting where it is edited, not only after saving it.
function labelSettingControls() {
  let n = 0;
  document.querySelectorAll('.set-row').forEach(row => {
    const title = row.querySelector(':scope > .set-info .set-info-t');
    if (!title) return;
    if (!title.id) title.id = `set-t-${++n}`;
    const desc = row.querySelector(':scope > .set-info .set-info-d');
    if (desc && !desc.id) desc.id = `set-d-${n}`;
    const more = row.querySelector(':scope > .set-info .set-info-more');
    const described = [desc?.id, more?.id].filter(Boolean).join(' ');
    row.querySelectorAll('input:not([type="hidden"]), select, textarea').forEach(ctrl => {
      if (ctrl.closest('.source-priority-row, .radio-discovery-row')) return;
      // A switch's <label> wraps only the track and thumb, so it gives no name of its own.
      const namedByLabel = Array.from(ctrl.labels || []).some(label => label.textContent.trim());
      if (namedByLabel || ctrl.hasAttribute('aria-label') || ctrl.hasAttribute('aria-labelledby')) return;
      ctrl.setAttribute('aria-labelledby', title.id);
      if (described) ctrl.setAttribute('aria-describedby', described);
    });
    if (row.querySelector('[data-restart="true"]') && !title.querySelector('.restart-badge')) {
      title.insertAdjacentHTML('beforeend',
        ' <span class="restart-badge restart-badge-inline" title="Takes effect after Octo restarts">Restart</span>');
    }
  });
}

// ────────────────────────────────────────────────────────────────
// Setup checklist (Status)
// ────────────────────────────────────────────────────────────────
// Built only from answers Octo already gives (status, settings, library status), so it can
// never disagree with the pages it links to.
function heartSourceReachable() {
  const services = lastStatus?.services || {};
  const steps = currentSettings?.Subsonic?.HeartDownloadSources || [];
  const enabled = steps.filter(step => step.SongEnabled || step.AlbumEnabled).map(step => step.Source);
  if (!enabled.length) return { ok: false, detail: 'No heart source is switched on, so a heart downloads nothing.' };
  const reachable = enabled.filter(source => {
    if (source === 'Soulseek') return services.slskd?.ok && !services.slskd?.warning;
    if (source === 'YouTube') return services.ytDlpShim?.ok;
    if (source === 'Lidarr') return services.lidarr?.ok && services.lidarr?.configured !== false && !services.lidarr?.warning;
    return false;
  });
  return reachable.length
    ? { ok: true, detail: `${reachable.join(', ')} answering.` }
    : { ok: false, detail: `${enabled.join(', ')} ${enabled.length === 1 ? 'is' : 'are'} switched on but not answering.` };
}

function renderSetupChecklist() {
  const holder = document.getElementById('setup-checklist');
  const summary = document.getElementById('setup-summary');
  const wrap = document.getElementById('setup-checklist-wrap');
  if (!holder || !summary || !wrap) return;
  const services = lastStatus?.services;
  const pending = { state: 'pending', detail: 'Checking…' };
  const heart = services && currentSettings ? heartSourceReachable() : null;
  const notificationsOn = !!(currentSettings?.Notifications?.NtfyUrl || currentSettings?.Notifications?.DiscordWebhookUrl);

  const rows = [
    {
      title: 'Music server', required: true, tab: 'subsonic',
      ...(services ? (services.navidrome?.ok
        ? { state: 'ok', detail: 'Navidrome answers.' }
        : { state: 'bad', detail: services.navidrome?.detail || 'Not answering.' }) : pending),
    },
    {
      title: 'A library folder Octo can write to', required: true, tab: 'library',
      ...(lastLibraryStatus ? (lastLibraryStatus.writable
        ? { state: 'ok', detail: `Downloads go to ${lastLibraryStatus.effectiveDownloadPath || 'the configured folder'}.` }
        : { state: 'bad', detail: 'Octo cannot write to the download folder.' }) : pending),
    },
    {
      title: 'A heart source that answers', required: true, tab: 'downloads',
      ...(heart ? { state: heart.ok ? 'ok' : 'bad', detail: heart.detail } : pending),
    },
    {
      title: 'Song discovery and radio', required: false, tab: 'lastfm',
      ...(services ? (services.lastfm?.configured === false
        ? { state: 'optional', detail: 'Optional. A Last.fm key adds songs you do not own to search, and radio.' }
        : services.lastfm?.ok ? { state: 'ok', detail: 'Last.fm answers.' }
          : { state: 'bad', detail: services.lastfm?.detail || 'Last.fm is not answering.' }) : pending),
    },
    {
      title: 'Notifications', required: false, tab: 'notifications',
      ...(currentSettings ? (notificationsOn
        ? { state: 'ok', detail: 'A transport is set. Send a test from Notifications.' }
        : { state: 'optional', detail: 'Optional. The only way to hear that a heart failed.' }) : pending),
    },
  ];

  const stateLabel = { ok: 'Done', bad: 'Needs attention', optional: 'Optional', pending: 'Checking' };
  const markup = `<ol class="setup-list">${rows.map(row => `
    <li class="setup-row">
      <span class="setup-state ${row.state}" aria-hidden="true"></span>
      <span class="setup-copy">
        <span class="setup-title">${esc(row.title)}<span class="visually-hidden">: ${stateLabel[row.state]}</span></span>
        <span class="setup-detail">${esc(row.detail)}</span>
      </span>
      <button type="button" class="btn ${row.state === 'bad' ? '' : 'btn-ghost '}btn-sm" data-open-tab="${row.tab}" aria-label="${row.state === 'bad' ? 'Fix' : 'Open'} ${esc(row.title)}">${row.state === 'bad' ? 'Fix' : 'Open'}${icon('i-caret-right')}</button>
    </li>`).join('')}</ol>`;
  // Only when something changed, so a status refresh does not pull focus off a button.
  if (holder.dataset.rendered !== markup) {
    holder.innerHTML = markup;
    holder.dataset.rendered = markup;
  }

  const required = rows.filter(row => row.required);
  const missing = required.filter(row => row.state === 'bad');
  const checking = required.some(row => row.state === 'pending');
  const summaryText = checking ? 'Checking setup…'
    : missing.length ? `Setup: ${missing.length} of ${required.length} required step${required.length === 1 ? '' : 's'} need${missing.length === 1 ? 's' : ''} attention`
      : 'Setup complete. Point your apps at the address below.';
  const done = required.filter(row => row.state === 'ok').length;
  const summaryMarkup = `<span class="setup-summary-text">${esc(summaryText)}</span>`
    + (checking ? '' : `<span class="setup-meter" role="img" aria-label="${done} of ${required.length} required steps done"><span style="width:${Math.round(100 * done / required.length)}%"></span></span>`);
  if (summary.innerHTML !== summaryMarkup) summary.innerHTML = summaryMarkup;
  wrap.classList.toggle('needs-attention', missing.length > 0);
  // The sidebar marks where each missing step is fixed, so setup is visible from every page.
  rows.filter(row => row.required).forEach(row => {
    const item = document.querySelector(`.sidebar-nav-item[data-tab="${row.tab}"]`);
    if (!item) return;
    const flagged = row.state === 'bad';
    let flag = item.querySelector('.nav-flag');
    if (flagged && !flag) {
      flag = document.createElement('span');
      flag.className = 'nav-flag';
      flag.innerHTML = '<span class="visually-hidden"> (needs setting up)</span>';
      item.appendChild(flag);
    } else if (!flagged && flag) {
      flag.remove();
    }
  });
  // Open when something required is missing; otherwise stay out of the way, unless the user
  // opened it themselves.
  if (!wrap.dataset.userToggled) wrap.open = missing.length > 0;
}
// Once the user opens or closes it themselves, stop opening and closing it for them.
document.getElementById('setup-summary')?.addEventListener('click', () => {
  document.getElementById('setup-checklist-wrap').dataset.userToggled = '1';
});

// ---- Lyrics sources ------------------------------------------------------
//
// The order is the saved LYRICS_SOURCES string: the sources that are on, top first. A source
// that is off keeps its place on the page until the next load, where it goes to the bottom.
const lyricsSourceMeta = {
  song: { title: "The song’s own lyrics", detail: 'Already in its tags or a file beside it. Always on; rank it to decide when they win.', fixed: true },
  kugou: { title: 'KuGou', detail: 'Word-timed lyrics for most songs. An unofficial API that can change without notice.' },
  lrclib: { title: 'LRCLIB', detail: 'Open and keyless. Timed line by line, now and then word by word.' },
  netease: { title: 'NetEase', detail: 'Deep on non-Western and older music. An unofficial API.' },
  lyricsovh: { title: 'lyrics.ovh', detail: 'Plain text only, the last resort.' },
};
let lyricsSources = [];

function renderLyricsSources(saved) {
  const list = document.getElementById('lyrics-source-order');
  if (!list) return;
  if (saved !== undefined) {
    const on = String(saved ?? '').split(',').map(name => name.trim().toLowerCase()).filter(name => lyricsSourceMeta[name]);
    // The song's own lyrics are always in the order: first when an older saved order leaves them out.
    const unique = [...new Set(on.includes('song') ? on : ['song', ...on])];
    lyricsSources = [
      ...unique.map(name => ({ name, on: true })),
      ...Object.keys(lyricsSourceMeta).filter(name => !unique.includes(name)).map(name => ({ name, on: false })),
    ];
  }
  list.innerHTML = lyricsSources.map((source, index) => {
    const meta = lyricsSourceMeta[source.name];
    return `
      <div class="source-priority-row${source.on ? '' : ' is-disabled'}" data-source="${source.name}" data-index="${index}">
        <button type="button" class="source-drag" draggable="true"
                aria-label="Drag ${esc(meta.title)} to reorder. Use arrow keys to move it."
                title="Drag to reorder; arrow keys also work">
          ${icon('i-dots-six-vertical')}
        </button>
        <span class="source-step" aria-hidden="true">${index + 1}</span>
        <span class="source-copy">
          <span class="source-title">${esc(meta.title)}</span>
          <span class="source-detail">${esc(meta.detail)}</span>
        </span>
        ${meta.fixed ? '' : `<label class="switch source-kind-switch">
          <input type="checkbox" data-lyrics-source-on aria-label="Use ${esc(meta.title)}" ${source.on ? 'checked' : ''} />
          <span class="sw-track"></span><span class="sw-thumb"></span>
        </label>`}
      </div>`;
  }).join('');
  syncLyricsSourcesInput();
}

function syncLyricsSourcesInput() {
  const input = document.getElementById('f-lyrics-sources');
  const help = document.getElementById('lyrics-source-order-help');
  const value = lyricsSources.filter(source => source.on).map(source => source.name).join(',');
  if (input && input.value !== value) {
    input.value = value;
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }
  const looksUp = lyricsSources.some(source => source.on && !lyricsSourceMeta[source.name].fixed);
  if (help) {
    help.hidden = looksUp;
    help.textContent = looksUp ? '' : 'Every source is off, so only the lyrics songs already have will show.';
  }
}

function moveLyricsSource(from, to) {
  if (from === to || to < 0 || to >= lyricsSources.length) return;
  const [source] = lyricsSources.splice(from, 1);
  lyricsSources.splice(to, 0, source);
  renderLyricsSources();
  document.querySelector(`#lyrics-source-order .source-priority-row[data-index="${to}"] .source-drag`)?.focus();
}

const lyricsSourceList = document.getElementById('lyrics-source-order');
let draggedLyricsRow = null;
lyricsSourceList?.addEventListener('change', event => {
  if (!event.target.matches('[data-lyrics-source-on]')) return;
  const row = event.target.closest('.source-priority-row');
  lyricsSources[Number(row.dataset.index)].on = event.target.checked;
  row.classList.toggle('is-disabled', !event.target.checked);
  syncLyricsSourcesInput();
});
lyricsSourceList?.addEventListener('keydown', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle || !['ArrowUp', 'ArrowDown'].includes(event.key)) return;
  event.preventDefault();
  const from = Number(handle.closest('.source-priority-row').dataset.index);
  moveLyricsSource(from, from + (event.key === 'ArrowUp' ? -1 : 1));
});
lyricsSourceList?.addEventListener('dragstart', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle) return;
  draggedLyricsRow = handle.closest('.source-priority-row');
  event.dataTransfer.effectAllowed = 'move';
  event.dataTransfer.setData('text/plain', draggedLyricsRow.dataset.source);
  draggedLyricsRow.classList.add('is-dragging');
});
lyricsSourceList?.addEventListener('dragover', event => {
  const target = event.target.closest('.source-priority-row');
  if (!target || !draggedLyricsRow || target === draggedLyricsRow) return;
  event.preventDefault();
  previewLyricsRowMove(target, event.clientY);
});
lyricsSourceList?.addEventListener('drop', event => event.preventDefault());
lyricsSourceList?.addEventListener('dragend', () => finishLyricsDrag());
// Touch and pen: the same move, by pointer, since those never fire drag events.
lyricsSourceList?.addEventListener('pointerdown', event => {
  const handle = event.target.closest('.source-drag');
  if (!handle || event.pointerType === 'mouse') return;
  event.preventDefault();
  draggedLyricsRow = handle.closest('.source-priority-row');
  draggedLyricsRow.classList.add('is-dragging');
  handle.setPointerCapture(event.pointerId);
});
lyricsSourceList?.addEventListener('pointermove', event => {
  if (!draggedLyricsRow || event.pointerType === 'mouse') return;
  const target = document.elementFromPoint(event.clientX, event.clientY)?.closest('#lyrics-source-order .source-priority-row');
  if (target) previewLyricsRowMove(target, event.clientY);
});
lyricsSourceList?.addEventListener('pointerup', event => { if (event.pointerType !== 'mouse') finishLyricsDrag(); });
lyricsSourceList?.addEventListener('pointercancel', event => { if (event.pointerType !== 'mouse') finishLyricsDrag(); });

function previewLyricsRowMove(target, clientY) {
  if (!draggedLyricsRow || target === draggedLyricsRow) return;
  const after = clientY > target.getBoundingClientRect().top + target.offsetHeight / 2;
  lyricsSourceList.insertBefore(draggedLyricsRow, after ? target.nextSibling : target);
}

function finishLyricsDrag() {
  if (!draggedLyricsRow) return;
  draggedLyricsRow.classList.remove('is-dragging');
  draggedLyricsRow = null;
  const byName = new Map(lyricsSources.map(source => [source.name, source]));
  lyricsSources = [...lyricsSourceList.querySelectorAll('.source-priority-row')]
    .map(row => byName.get(row.dataset.source)).filter(Boolean);
  renderLyricsSources();
}

// ---- Find lyrics for the library -----------------------------------------
//
// Gated on the Navidrome admin sign-in, as the genre backfill is: it writes files and names
// them. A 401 asks for the sign-in once and tries again.

let lyricsLibraryPoll = null;

async function lyricsFetch(url, options = {}, retry = true, holderId = 'lyrics-bar') {
  const response = await api(url, options);
  if (response.status === 401 && retry) {
    const holder = document.getElementById(holderId);
    if (await browseAuthenticate(holder ?? document.createElement('div'))) return lyricsFetch(url, options, false, holderId);
  }
  return response;
}

async function lyricsError(response) {
  try { return (await response.json()).error || `HTTP ${response.status}`; } catch { return `HTTP ${response.status}`; }
}

// ---- Better lyrics ------------------------------------------------------
//
// The soft covers wall's steps, for lyrics: Scan lists the songs with no lyrics or weaker ones and
// changes nothing, Find better lyrics looks the picked ones up and changes nothing, Save writes
// what was found, and Undo puts back everything Save wrote over.

let lyricsRun = null;
let lyricsPickedRows = new Set();
let lyricsListRunId = null;
let lyricsShowAll = false;
let lyricsControlsSet = false;
let lyricsPace = null;
const LYRICS_ROW_CAP = 400;
const lyricsEl = id => document.getElementById(id);
const lyricsHasLabel = { none: 'No lyrics', plain: 'Not timed', line: 'Timed by line', word: 'Word by word' };
const lyricsKindLabel = { word: 'Word by word', line: 'Timed by line', plain: 'Not timed', instrumental: 'Instrumental' };
const lyricsSaveLabel = { beside: 'beside each song', inside: 'inside each song', both: 'beside and inside each song' };

function lyricsNote(message, kind = 'ok') {
  note(lyricsEl('lyrics-head')?.closest('.cover-summary'), message, kind);
}

// Set when a step was started from this page, so the page keeps watching (and says so) even
// while a status request fails, instead of going quiet.
let lyricsExpectRunning = false;
let lyricsLoadError = '';

// A scan lists every song it found wanting (the Show chip can narrow it to songs with none);
// later steps list the songs they worked on.
function lyricsRows(run) {
  let rows = run?.rows || [];
  if (run?.mode && run.mode !== 'Scan') rows = rows.filter(row => row.result !== 'weak');
  if (lyricsEl('lyrics-show')?.value === 'none') rows = rows.filter(row => row.has === 'none');
  return rows;
}

function lyricsPickable(run, row) {
  if (!run || run.status === 'Running' || run.mode === 'Undo') return false;
  if (run.mode === 'Scan') return true;
  if (run.mode === 'Preview') return row.result === 'found';
  return false;
}

function lyricsItem(run, row) {
  const pickable = lyricsPickable(run, row);
  const picked = pickable && lyricsPickedRows.has(row.id);
  const badges = [`<span class="lyrics-badge">${esc(lyricsHasLabel[row.has] || row.has)}</span>`];
  const kind = lyricsKindLabel[row.kind] || row.kind || '';
  if (row.result === 'found') {
    badges.push(row.doubt
      ? `<span class="lyrics-badge warn" title="${esc(row.doubt)}">${esc(kind)} · ${esc(row.source)} · not certain</span>`
      : `<span class="lyrics-badge good">${esc(kind)} · ${esc(row.source)}</span>`);
  } else if (row.result === 'saved') badges.push(`<span class="lyrics-badge done">${icon('i-check')} ${esc(kind)} · saved</span>`);
  else if (row.result === 'none') badges.push(`<span class="lyrics-badge">${row.kind === 'instrumental' ? 'Instrumental' : 'Nothing better'}</span>`);
  else if (row.result === 'busy') badges.push('<span class="lyrics-badge warn">No answer, try again</span>');
  else if (row.result === 'kept') badges.push('<span class="lyrics-badge">Already as good</span>');
  else if (row.result === 'blocked') badges.push('<span class="lyrics-badge warn" title="Its lyrics file is one Octo did not write, so Octo leaves it alone.">Has lyrics Octo will not replace</span>');
  else if (row.result === 'failed') badges.push('<span class="lyrics-badge warn">Could not read the file</span>');
  const preview = row.preview?.length && ['found', 'saved'].includes(row.result)
    ? `<span class="lyrics-preview">${row.preview.map(esc).join(' / ')}</span>` : '';
  return `<div class="lyrics-item" role="listitem">
    <button type="button" class="lyrics-pick" data-lyrics-row="${esc(row.id)}" title="${esc(row.path)}"
        ${pickable ? `role="checkbox" aria-checked="${picked}"` : 'aria-disabled="true"'}
        aria-label="${esc(`${row.title} by ${row.artist}`)}">
      <span class="lyrics-check">${icon('i-check')}</span>
      <span class="lyrics-song">
        <span class="lyrics-title">${esc(row.title)}</span>
        <span class="set-info-d">${esc([row.artist, row.album].filter(Boolean).join(' · '))}</span>
        ${preview}
      </span>
      <span class="lyrics-badges">${badges.join('')}</span>
    </button>
    <button type="button" class="link-btn" data-lyrics-choose="${esc(row.path)}">Choose</button>
  </div>`;
}

function renderLyricsBar(run) {
  const rows = lyricsRows(run);
  const running = run.status === 'Running';
  const pickableRows = rows.filter(row => lyricsPickable(run, row));
  const count = pickableRows.filter(row => lyricsPickedRows.has(row.id)).length;
  const go = lyricsEl('lyrics-go');
  const hint = lyricsEl('lyrics-hint');
  const countEl = lyricsEl('lyrics-count');

  lyricsEl('lyrics-stop').hidden = !running || run.mode === 'Undo';
  // A scan is quick to start over; lookups and saves are worth resuming.
  lyricsEl('lyrics-resume').hidden = running || !run.canResume || run.mode === 'Scan';
  lyricsEl('lyrics-undo').hidden = running || !run.canUndo;
  lyricsEl('lyrics-select').hidden = pickableRows.length === 0;
  go.hidden = true;
  countEl.textContent = '';
  hint.textContent = '';

  if (running) {
    hint.textContent = {
      Scan: 'Reading your songs. Nothing is looked up or changed.',
      Preview: 'Looking up each picked song, a couple of seconds apiece. Nothing changes yet.',
      Save: 'Saving lyrics. Whatever they replace is kept, so you can undo.',
      Undo: 'Putting the old lyrics back.',
    }[run.mode] || 'Working.';
  } else if (run.mode === 'Scan' && pickableRows.length) {
    countEl.textContent = `${count.toLocaleString()} selected`;
    // A lookup and the pause after it come to about 2.5 seconds a song.
    const minutes = Math.round(count * 2.5 / 60);
    const takes = minutes >= 90 ? `about ${Math.round(minutes / 60)} hours` : minutes >= 2 ? `about ${minutes} minutes` : 'a minute or two';
    hint.textContent = `Looks up better lyrics for each, ${takes}. Nothing changes yet, and you can stop and resume.`;
    go.textContent = 'Find better lyrics';
    go.hidden = false;
  } else if (run.mode === 'Preview' && pickableRows.length) {
    countEl.textContent = `${count.toLocaleString()} selected`;
    hint.textContent = `Saved ${lyricsSaveLabel[run.saveTo] || 'beside each song'}. Whatever they replace is kept, so you can undo.`;
    go.textContent = `Save ${count} lyric${count === 1 ? '' : 's'}`;
    go.hidden = false;
  } else if (run.mode === 'Save' && run.status === 'Completed') {
    hint.textContent = !run.written ? 'Nothing needed saving.'
      : run.saveTo === 'beside' ? 'Every app shows them now.' : 'Navidrome picks up lyrics inside songs at its next scan, which Octo asked for.';
  } else if (run.mode === 'Undo' && run.status === 'Completed') {
    hint.textContent = run.reason || 'The old lyrics are back.';
  } else if (run.status === 'Cancelled' || run.status === 'Interrupted') {
    hint.textContent = run.mode === 'Scan' ? 'Stopped part way. Scan again to see every song.' : (run.reason || 'Stopped.');
  }
  lyricsEl('lyrics-bar').hidden = run.status === 'Idle'
    || (go.hidden && lyricsEl('lyrics-stop').hidden && lyricsEl('lyrics-resume').hidden && lyricsEl('lyrics-undo').hidden && !hint.textContent);
}

function renderLyricsLibrary(run) {
  if (!lyricsEl('lyrics-list')) return;
  const rows = lyricsRows(run);
  const running = run.status === 'Running';

  // A fresh list from a finished step: pick everything worth carrying on. Lyrics the sources
  // were not sure of are shown, not pre-picked.
  if (!running && run.runId && run.runId !== lyricsListRunId) {
    lyricsListRunId = run.runId;
    lyricsShowAll = false;
    lyricsPickedRows = new Set(rows.filter(row => lyricsPickable(run, row) && !row.doubt).map(row => row.id));
  }

  const progress = lyricsEl('lyrics-progress');
  progress.hidden = !running;
  const starting = running && (run.starting || !run.total);
  progress.classList.toggle('starting', starting);
  if (running && !starting) lyricsEl('lyrics-progress-fill').style.width = `${run.total ? Math.min(100, Math.round(100 * run.processed / run.total)) : 0}%`;

  const plural = (n, one, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
  const all = run.rows || [];
  const countHas = has => all.filter(row => row.has === has).length;
  let head = '';
  let sub = '';
  if (run.status === 'Idle') head = '';
  else if (starting) {
    head = 'Getting started';
    sub = 'Listing your songs';
  } else if (running) {
    head = { Scan: 'Scanning', Preview: 'Finding better lyrics', Save: 'Saving lyrics', Undo: 'Putting lyrics back', Walk: 'Finding lyrics' }[run.mode] || 'Working';
    sub = `${run.processed.toLocaleString()} of ${plural(run.total, 'song')}`;
    if (run.mode === 'Scan' && all.length) sub += ` · ${all.length.toLocaleString()} could have better lyrics so far`;
    if (run.mode === 'Preview' && run.upgraded) sub += ` · ${run.upgraded.toLocaleString()} found so far`;
    if (lyricsPace?.runId !== run.runId) lyricsPace = { runId: run.runId, at: Date.now(), done: run.processed };
    const moved = run.processed - lyricsPace.done;
    if (moved >= 3) {
      const left = (Date.now() - lyricsPace.at) / moved * Math.max(0, run.total - run.processed) / 1000;
      if (left >= 5) sub += left < 90 ? ` · about ${Math.round(left / 5) * 5} s left` : ` · about ${Math.round(left / 60)} min left`;
    }
  } else if (run.mode === 'Scan') {
    head = all.length ? plural(all.length, 'song could have better lyrics', 'songs could have better lyrics') : 'Every song has word-by-word lyrics';
    const n = value => value.toLocaleString();
    sub = [countHas('none') ? `${n(countHas('none'))} with none` : '', countHas('plain') ? `${n(countHas('plain'))} not timed` : '',
      countHas('line') ? `${n(countHas('line'))} timed by line` : '', run.wordAlready ? `${n(run.wordAlready)} already word by word` : '']
      .filter(Boolean).join(' · ');
  } else if (run.mode === 'Preview') {
    const found = all.filter(row => row.result === 'found');
    head = found.length ? `Better lyrics found for ${plural(found.length, 'song')}` : 'No better lyrics found';
    const unsure = found.filter(row => row.doubt).length;
    sub = [run.notFound ? `${run.notFound} with nothing better` : '', unsure ? `${unsure} not certain, left unpicked` : '',
      run.busy ? `${run.busy} not answered` : ''].filter(Boolean).join(' · ');
  } else if (run.mode === 'Save') {
    head = run.written ? `Lyrics saved for ${plural(run.written, 'song')}` : 'Nothing saved';
    sub = [run.alreadyHad ? `${run.alreadyHad} already as good` : '', run.skipped ? `${run.skipped} with lyrics Octo will not replace` : '',
      run.failed ? `${run.failed} failed` : ''].filter(Boolean).join(' · ');
  } else if (run.mode === 'Undo') {
    head = run.written ? `Old lyrics put back for ${plural(run.written, 'file')}` : 'Nothing put back';
  } else {
    head = 'Library walk';
    sub = `${run.written} written · ${run.notFound} not found`;
  }
  if (!running && ['Cancelled', 'Interrupted', 'Failed'].includes(run.status)) head = `${head || 'Stopped'} (${run.status.toLowerCase()})`;
  lyricsEl('lyrics-head').textContent = head;
  lyricsEl('lyrics-sub').textContent = sub;

  lyricsEl('lyrics-empty').hidden = run.status !== 'Idle';
  const shown = lyricsShowAll ? rows : rows.slice(0, LYRICS_ROW_CAP);
  lyricsEl('lyrics-list').innerHTML = shown.map(row => lyricsItem(run, row)).join('');
  const more = lyricsEl('lyrics-more');
  const lastError = run.errors?.length ? run.errors[run.errors.length - 1] : '';
  more.hidden = !(rows.length > shown.length || lastError);
  more.innerHTML = [
    rows.length > shown.length ? `Showing ${shown.length} of ${rows.length}. <button type="button" class="link-btn" id="lyrics-show-all">Show all</button> Select all includes the rest.` : '',
    lastError ? `<span class="field-error">${esc(lastError)}</span>` : '',
  ].filter(Boolean).join(' ');
  lyricsEl('lyrics-scan').disabled = running;
  renderLyricsBar(run);
}

function lyricsWatch(on) {
  if (on && !lyricsLibraryPoll) lyricsLibraryPoll = setInterval(() => loadLyricsLibrary(), 1500);
  if (!on && lyricsLibraryPoll) {
    clearInterval(lyricsLibraryPoll);
    lyricsLibraryPoll = null;
  }
}

// What the page shows when Octo could not say how the lyrics run stands.
function renderLyricsTrouble(message, signIn) {
  if (!lyricsEl('lyrics-list')) return;
  lyricsEl('lyrics-head').textContent = signIn ? '' : lyricsExpectRunning ? 'Still working' : 'Could not load the lyrics scan';
  lyricsEl('lyrics-sub').textContent = signIn ? '' : message;
  if (signIn && !lyricsRun) {
    lyricsEl('lyrics-empty').hidden = false;
    lyricsEl('lyrics-empty').querySelector('.set-info-d').textContent =
      'Sign in with your Navidrome admin account to scan. A scan only reads your songs: it looks nothing up and changes nothing.';
  }
}

async function loadLyricsLibrary(retry = false) {
  let response;
  try {
    response = await lyricsFetch('/api/admin/lyrics/library', {}, retry);
  } catch (error) {
    response = null;
  }
  if (!response?.ok) {
    const signIn = response?.status === 401;
    lyricsLoadError = signIn ? '' : response ? await lyricsError(response) : 'Octo did not answer.';
    renderLyricsTrouble(lyricsExpectRunning
      ? `Octo did not say how far it got (${lyricsLoadError}). Checking again.`
      : lyricsLoadError, signIn);
    // A step started from here goes on on the server; keep asking until it can be shown.
    lyricsWatch(lyricsExpectRunning && !signIn);
    return null;
  }
  lyricsLoadError = '';
  const run = await response.json();
  // Accepted but not started yet: shown as starting, so the page keeps watching.
  if (run.running && run.status !== 'Running') Object.assign(run, { status: 'Running', starting: true });
  if (run.status !== 'Running') lyricsExpectRunning = false;
  const previous = lyricsRun;
  lyricsRun = run;
  if (!lyricsControlsSet && run.status !== 'Idle' && run.scope) {
    lyricsEl('lyrics-scope').value = run.scope;
    document.querySelectorAll('.seg[data-seg-for="lyrics-scope"]').forEach(seg => syncSegment(seg, false));
  }
  lyricsControlsSet = true;
  renderLyricsLibrary(run);
  if (previous?.status === 'Running' && run.status === 'Failed') lyricsNote(run.reason || 'The run stopped.', 'error');

  lyricsWatch(run.status === 'Running');
  return run;
}

async function startLyrics(mode, picked = null) {
  const scope = picked ? lyricsRun?.scope : lyricsEl('lyrics-scope').value;
  const response = await lyricsFetch('/api/admin/lyrics/library', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ mode, scope, picked }),
  });
  if (!response.ok) {
    lyricsNote(await lyricsError(response), 'error');
    return;
  }
  // Said at once, before Octo's first answer: the step has been handed over.
  lyricsNote('');
  lyricsExpectRunning = true;
  const base = lyricsRun || { rows: [], errors: [] };
  renderLyricsLibrary({ ...base, runId: base.runId, mode, status: 'Running', starting: true, total: 0, processed: 0,
    rows: mode === 'Scan' ? [] : base.rows });
  lyricsWatch(true);
  await loadLyricsLibrary();
}

function lyricsPickedIds() {
  return lyricsRows(lyricsRun).filter(row => lyricsPickable(lyricsRun, row) && lyricsPickedRows.has(row.id)).map(row => row.id);
}

lyricsEl('lyrics-scan')?.addEventListener('click', () => startLyrics('Scan'));
lyricsEl('lyrics-scan-first')?.addEventListener('click', () => startLyrics('Scan'));
lyricsEl('lyrics-show')?.addEventListener('change', () => { if (lyricsRun) renderLyricsLibrary(lyricsRun); });
lyricsEl('lyrics-list')?.addEventListener('click', event => {
  const choose = event.target.closest('[data-lyrics-choose]');
  if (choose) {
    openLyricsPicker({ path: choose.dataset.lyricsChoose });
    document.getElementById('lyrics-picker')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    return;
  }
  const item = event.target.closest('.lyrics-pick[role="checkbox"]');
  if (!item || !lyricsRun) return;
  const id = item.dataset.lyricsRow;
  if (lyricsPickedRows.has(id)) lyricsPickedRows.delete(id); else lyricsPickedRows.add(id);
  item.setAttribute('aria-checked', String(lyricsPickedRows.has(id)));
  renderLyricsBar(lyricsRun);
});
lyricsEl('lyrics-more')?.addEventListener('click', event => {
  if (event.target.id !== 'lyrics-show-all' || !lyricsRun) return;
  lyricsShowAll = true;
  renderLyricsLibrary(lyricsRun);
});
lyricsEl('lyrics-all')?.addEventListener('click', () => {
  lyricsPickedRows = new Set(lyricsRows(lyricsRun).filter(row => lyricsPickable(lyricsRun, row)).map(row => row.id));
  renderLyricsLibrary(lyricsRun);
});
lyricsEl('lyrics-none')?.addEventListener('click', () => {
  lyricsPickedRows = new Set();
  renderLyricsLibrary(lyricsRun);
});
lyricsEl('lyrics-go')?.addEventListener('click', async () => {
  const run = lyricsRun;
  const ids = lyricsPickedIds();
  if (!run || !ids.length) { lyricsNote('Pick at least one song first.', 'info'); return; }
  if (run.mode === 'Scan') { await startLyrics('Preview', ids); return; }
  if (run.mode === 'Preview') {
    if (!(await askConfirm(`Save lyrics for ${ids.length} song${ids.length === 1 ? '' : 's'}?`,
      'Octo replaces only lyrics it wrote, and keeps what it replaces, so Undo puts it back.', 'Save lyrics'))) return;
    await startLyrics('Save', ids);
  }
});
lyricsEl('lyrics-stop')?.addEventListener('click', async () => {
  const response = await lyricsFetch('/api/admin/lyrics/library/cancel', { method: 'POST' });
  if (!response.ok) { lyricsNote('Could not stop.', 'error'); return; }
  lyricsNote('Stopping after the current song.', 'info');
  await loadLyricsLibrary();
});
lyricsEl('lyrics-resume')?.addEventListener('click', async () => {
  const response = await lyricsFetch('/api/admin/lyrics/library/resume', { method: 'POST' });
  if (!response.ok) { lyricsNote(await lyricsError(response), 'error'); return; }
  await loadLyricsLibrary();
});
lyricsEl('lyrics-undo')?.addEventListener('click', async () => {
  if (!(await askConfirm('Put back the old lyrics?', 'Every lyrics file and tag Save wrote goes back to what it was. Anything changed since is left alone.', 'Put back lyrics'))) return;
  await startLyrics('Undo');
});

// ---- Fix a song's lyrics -------------------------------------------------

let lyricsPicked = null;
const lyricsChoiceLabel = choice => choice === 'auto' ? 'Automatic' : choice === 'none' ? 'Hidden' : 'Chosen by hand';

async function searchLyricsSongs() {
  const q = document.getElementById('lyrics-song-search')?.value.trim();
  const results = document.getElementById('lyrics-song-results');
  if (!q || !results) return;
  results.innerHTML = stateBlock('loading', 'Searching…');
  const response = await lyricsFetch(`/api/admin/lyrics/songs?q=${encodeURIComponent(q)}`, {}, true, 'lyrics-song-results');
  if (!response.ok) {
    results.innerHTML = stateBlock('error', await lyricsError(response));
    return;
  }
  const { songs } = await response.json();
  results.innerHTML = songs.length ? songs.map(song => `
    <div class="config-row lyrics-row">
      <span class="lyrics-song"><strong>${esc(song.title)}</strong> <span class="set-opt">${esc(song.artist)}</span>
        <span class="set-info-d">${esc(song.album ?? '')}${song.choice !== 'auto' ? ` · ${lyricsChoiceLabel(song.choice)}` : ''}</span></span>
      <span><button class="btn btn-ghost" type="button" data-lyrics-song="${esc(song.id)}">Lyrics…</button></span>
    </div>`).join('') : stateBlock('empty', 'No songs found.');
}

document.getElementById('lyrics-song-search-go')?.addEventListener('click', searchLyricsSongs);
document.getElementById('lyrics-song-search')?.addEventListener('keydown', event => {
  if (event.key === 'Enter') { event.preventDefault(); searchLyricsSongs(); }
});
document.getElementById('lyrics-song-results')?.addEventListener('click', event => {
  const button = event.target.closest('[data-lyrics-song]');
  if (button) openLyricsPicker({ id: button.dataset.lyricsSong });
});

async function openLyricsPicker(target, manual = null) {
  const picker = document.getElementById('lyrics-picker');
  const list = document.getElementById('lyrics-candidates');
  if (!picker || !list) return;
  picker.hidden = false;
  list.innerHTML = stateBlock('loading', 'Asking every lyrics source. This takes a few seconds.');
  picker.scrollIntoView({ behavior: 'smooth', block: 'nearest' });

  const query = new URLSearchParams();
  if (target.id) query.set('id', target.id);
  if (target.path) query.set('path', target.path);
  if (manual?.artist) query.set('artist', manual.artist);
  if (manual?.title) query.set('title', manual.title);
  const response = await lyricsFetch(`/api/admin/lyrics/candidates?${query}`, {}, true, 'lyrics-candidates');
  if (!response.ok) {
    list.innerHTML = stateBlock('error', await lyricsError(response));
    return;
  }
  const song = await response.json();
  lyricsPicked = { id: song.id, path: song.path };
  document.getElementById('lyrics-picker-title').textContent = `${song.title} · ${song.artist}`;
  document.getElementById('lyrics-picker-state').textContent = `Now: ${lyricsChoiceLabel(song.choice)}.`;
  if (!manual) {
    document.getElementById('lyrics-picker-artist').value = song.artist ?? '';
    document.getElementById('lyrics-picker-song').value = song.title ?? '';
  }
  document.getElementById('lyrics-picker-hide').disabled = !song.id;

  list.innerHTML = song.candidates.length ? song.candidates.map(candidate => `
    <div class="config-row lyrics-row${candidate.id === song.choice ? ' is-chosen' : ''}">
      <span class="lyrics-song">
        <strong>${esc(candidate.title)}</strong> <span class="set-opt">${esc(candidate.artist)}</span>
        <span class="set-info-d">${esc(lyricsSourceMeta[candidate.source]?.title ?? candidate.source)} · ${esc({ word: 'timed word by word', line: 'timed line by line', plain: 'not timed', instrumental: 'instrumental' }[candidate.kind] ?? candidate.kind)}${candidate.album ? ` · ${esc(candidate.album)}` : ''}${candidate.duration ? ` · ${Math.floor(candidate.duration / 60)}:${String(candidate.duration % 60).padStart(2, '0')}` : ''}${candidate.sameSong ? '' : ' · <em>may be another song</em>'}</span>
        <span class="lyrics-preview">${candidate.preview.map(esc).join('<br>')}</span>
      </span>
      <span><button class="btn btn-ghost" type="button" data-lyrics-candidate="${esc(candidate.id)}" ${candidate.kind === 'instrumental' ? 'disabled' : ''}>${candidate.id === song.choice ? 'In use' : 'Use these'}</button></span>
    </div>`).join('') : stateBlock('empty', 'No source has lyrics for this. Try another title or artist above.');
}

async function chooseLyrics(candidate) {
  if (!lyricsPicked) return;
  const response = await lyricsFetch('/api/admin/lyrics/choice', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id: lyricsPicked.id, path: lyricsPicked.path, candidate }),
  }, true, 'lyrics-candidates');
  const picker = document.getElementById('lyrics-picker');
  const where = picker && !picker.hidden
    ? picker.querySelector('.genre-preset-actions')
    : document.getElementById('lyrics-choices-list');
  if (!response.ok) {
    note(where, await lyricsError(response), 'error');
    return;
  }
  note(where, candidate === 'none' ? 'Lyrics hidden for this song.' : candidate === 'auto' ? 'Back to automatic lyrics.' : 'Lyrics chosen for this song.');
  document.getElementById('lyrics-picker-state').textContent = `Now: ${lyricsChoiceLabel(candidate === 'none' || candidate === 'auto' ? candidate : 'pinned')}.`;
  loadLyricsChoices();
  loadLyricsLibrary();
}

document.getElementById('lyrics-candidates')?.addEventListener('click', event => {
  const button = event.target.closest('[data-lyrics-candidate]');
  if (button) chooseLyrics(button.dataset.lyricsCandidate);
});
document.getElementById('lyrics-picker-hide')?.addEventListener('click', () => chooseLyrics('none'));
document.getElementById('lyrics-picker-auto')?.addEventListener('click', () => chooseLyrics('auto'));
document.getElementById('lyrics-picker-search')?.addEventListener('click', () => {
  if (!lyricsPicked) return;
  openLyricsPicker(lyricsPicked, {
    artist: document.getElementById('lyrics-picker-artist')?.value.trim(),
    title: document.getElementById('lyrics-picker-song')?.value.trim(),
  });
});

async function loadLyricsChoices(retry = false) {
  const response = await lyricsFetch('/api/admin/lyrics/choices', {}, retry, 'lyrics-choices-list');
  if (!response.ok) return;
  const { choices } = await response.json();
  const wrap = document.getElementById('lyrics-choices');
  const list = document.getElementById('lyrics-choices-list');
  if (!wrap || !list) return;
  wrap.hidden = !choices.length;
  list.innerHTML = choices.slice(0, 100).map(choice => `
    <div class="config-row lyrics-row">
      <span class="lyrics-song"><strong>${esc(choice.title ?? choice.id)}</strong> <span class="set-opt">${esc(choice.artist ?? '')}</span>
        <span class="set-info-d">${choice.choice === 'none' ? 'Hidden' : `${esc(lyricsSourceMeta[choice.source?.toLowerCase()]?.title ?? choice.source ?? '')}, ${esc(choice.kind)}`}${choice.setBy ? ` · by ${esc(choice.setBy)}` : ''}</span></span>
      <span><button class="btn btn-ghost" type="button" data-lyrics-reset="${esc(choice.id)}">Back to automatic</button></span>
    </div>`).join('');
}

document.getElementById('lyrics-choices-list')?.addEventListener('click', async event => {
  const button = event.target.closest('[data-lyrics-reset]');
  if (!button) return;
  lyricsPicked = { id: button.dataset.lyricsReset };
  await chooseLyrics('auto');
});

// ────────────────────────────────────────────────────────────────
// Better quality: the library's lossy songs, found again in higher quality
// ────────────────────────────────────────────────────────────────
const lossy = { rows: [], picked: new Set(), shown: 200, timer: null, view: null, open: 0 };
const LOSSY_PAGE = 200;
const LOSSY_RECENT_MS = 28 * 24 * 3600 * 1000;
const upgradeWords = {
  queued: 'Queued', waiting: 'Waiting for Soulseek', working: 'Looking', upgraded: 'Upgraded',
  notFound: 'No higher quality found', rehearsed: 'Rehearsed', skipped: 'Skipped', failed: 'Failed',
};
const upgradeTone = { upgraded: 'good', waiting: 'warn', notFound: 'warn', skipped: 'warn', failed: 'failed' };
const isOpenJob = job => ['queued', 'waiting', 'working'].includes(job?.state);

// The page's reads and writes need the Navidrome admin sign-in, like the library actions history.
async function lossyFetch(path, options = {}) {
  let response = await api(path, { credentials: 'same-origin', ...options });
  if (response.status === 401 && await browseAuthenticate(document.getElementById('lossy-list'))) {
    response = await api(path, { credentials: 'same-origin', ...options });
  }
  return response;
}

async function loadLossy(refresh = false) {
  const list = document.getElementById('lossy-list');
  if (!list) return;
  list.innerHTML = stateBlock('loading', 'Reading your library…');
  try {
    const response = await lossyFetch(`/api/admin/lossy${refresh ? '?refresh=true' : ''}`);
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || body.title || `HTTP ${response.status}`);
    lossy.rows = body.songs || [];
    lossy.shown = LOSSY_PAGE;
    const formats = [...new Set(lossy.rows.map(row => (row.suffix || '').toLowerCase()).filter(Boolean))].sort();
    const select = document.getElementById('lossy-format');
    const current = select.value;
    select.innerHTML = '<option value="">Every format</option>'
      + formats.map(f => `<option value="${esc(f)}">${esc(f.toUpperCase())}</option>`).join('');
    select.value = formats.includes(current) ? current : '';
    await loadUpgrades();
  } catch (error) {
    list.innerHTML = stateBlock('error', error.message);
  }
}

async function loadUpgrades() {
  clearTimeout(lossy.timer);
  const response = await lossyFetch('/api/admin/upgrades');
  if (!response.ok) { renderLossy(); return; }
  lossy.view = await response.json();
  const jobs = new Map((lossy.view.jobs || []).map(job => [job.id, job]));
  for (const row of lossy.rows) {
    const job = jobs.get(row.id);
    if (job) row.job = job;
  }
  const open = (lossy.view.jobs || []).filter(isOpenJob).length;
  const finishedSome = lossy.open > 0 && open < lossy.open;
  lossy.open = open;
  renderLossy();
  // A song that became lossless leaves the list, so the list is read again once work ends.
  if (finishedSome && open === 0) { loadLossy(true); return; }
  if (open > 0 && document.querySelector('section[data-pane="lossy"].active')) {
    lossy.timer = setTimeout(loadUpgrades, 2000);
  }
}

function lossyVisible() {
  const q = document.getElementById('lossy-q').value.trim().toLowerCase();
  const format = document.getElementById('lossy-format').value;
  const youTubeOnly = document.getElementById('lossy-youtube').checked;
  const hideTried = document.getElementById('lossy-hide-tried').checked;
  const now = Date.now();
  return lossy.rows.filter(row =>
    (!q || `${row.title} ${row.artist} ${row.album ?? ''}`.toLowerCase().includes(q))
    && (!format || (row.suffix || '').toLowerCase() === format)
    && (!youTubeOnly || row.fromYouTube)
    && (!hideTried || !row.lastTried || now - Date.parse(row.lastTried.atUtc) > LOSSY_RECENT_MS));
}

function lossyGateClosed() {
  const gate = lossy.view?.gate;
  if (!gate) return 'Sign in with your Navidrome admin account to use this page.';
  if (lossy.view.sourceReady === false) {
    return `Better quality looks for copies on ${lossy.view.source}, which is not set up on this server.`;
  }
  if (!gate.enabled || !gate.betterQuality || gate.dryRun || !gate.allowed) {
    return `Better quality needs library actions on, the Better quality action on, you (${gate.user}) on the allowed list, and rehearsal mode off.`;
  }
  return null;
}

// What a running upgrade is doing, from its download's own row.
const stageWords = {
  Queued: 'Waiting for a download slot', Searching: 'Searching', Downloading: 'Downloading',
  Verifying: 'Checking it is the same song and really lossless', Importing: 'Swapping it in',
};
const mb = bytes => typeof bytes === 'number' && bytes > 0 ? `${(bytes / 1048576).toFixed(1)} MB` : '';

function lossyStage(job) {
  const pct = typeof job.progress === 'number' ? Math.round(job.progress * 100) : null;
  let words = stageWords[job.stage] ?? 'Starting';
  // The download's own source, never a name the page assumes.
  const source = job.source ?? lossy.view?.source;
  if (source && job.stage === 'Searching') words += ` ${source}`;
  if (source && job.stage === 'Downloading') words += ` from ${source}`;
  if (job.stage === 'Downloading' && pct !== null) {
    words += ` ${pct}%`;
    if (job.bytesTotal) words += `, ${mb(job.bytesDone)} of ${mb(job.bytesTotal)}`;
  }
  const meter = pct !== null ? `<span class="lossy-meter"><span style="width:${pct}%"></span></span>` : '';
  return `<span class="dl-badge state">Working</span><span class="lossy-detail">${esc(words)}${job.note ? ` · ${esc(job.note)}` : ''}</span>${meter}`;
}

function lossyStatus(row) {
  if (row.job) {
    if (row.job.state === 'working') return lossyStage(row.job);
    const word = upgradeWords[row.job.state] ?? row.job.state;
    const tone = upgradeTone[row.job.state] ?? 'state';
    const line = row.job.state === 'queued' ? '' : (row.job.detail ?? '');
    return `<span class="dl-badge ${tone}">${esc(word)}</span>${line ? `<span class="lossy-detail">${esc(line)}</span>` : ''}`;
  }
  if (row.lastTried) {
    return `<span class="lossy-sub">Tried ${esc(new Date(row.lastTried.atUtc).toLocaleDateString())}: ${esc(row.lastTried.outcome)}</span>`;
  }
  return '';
}

function renderLossy() {
  const list = document.getElementById('lossy-list');
  const visible = lossyVisible();
  const view = lossy.view;
  const youTube = lossy.rows.filter(row => row.fromYouTube).length;
  const parts = [`${lossy.rows.length} songs are not lossless${youTube ? `, ${youTube} of them from YouTube` : ''}.`];
  const jobs = view?.jobs || [];
  const count = state => jobs.filter(job => job.state === state).length;
  const tally = [
    [count('working'), 'running'], [count('queued'), 'waiting'], [count('waiting'), 'waiting for Soulseek'],
    [count('upgraded'), 'upgraded'], [count('notFound'), 'with no copy found'], [count('failed'), 'failed'],
  ].filter(([n]) => n > 0).map(([n, label]) => `${n} ${label}`);
  if (tally.length) parts.push(`Queue: ${tally.join(', ')}.`);
  if (view?.source) parts.push(`Copies come from ${view.source}.`);
  if (view) parts.push(`${view.why}`);
  // Soulseek's state matters only while Soulseek is one of the sources, and with Lidarr beside
  // it an outage just means Lidarr is asked alone.
  const plan = view?.plan || ['Soulseek'];
  if (view?.soulseek?.warning && plan.includes('Soulseek'))
    parts.push(plan.includes('Lidarr') ? `${view.soulseek.detail} Lidarr is asked alone until it is back.` : view.soulseek.detail);
  document.getElementById('lossy-status').textContent = parts.join(' ');

  const closed = lossyGateClosed();
  const gate = document.getElementById('lossy-gate');
  gate.hidden = !closed;
  if (closed) {
    gate.innerHTML = `${esc(closed)} <button type="button" class="link-btn" id="lossy-open-actions">Open Library actions</button>`;
    document.getElementById('lossy-open-actions')?.addEventListener('click', () => openTab('libraryactions'));
  }

  if (!lossy.rows.length) {
    list.innerHTML = stateBlock('empty', 'Every song in your library is lossless.');
  } else if (!visible.length) {
    list.innerHTML = stateBlock('empty', 'No song matches these filters.');
  } else {
    const shown = visible.slice(0, lossy.shown);
    list.innerHTML = `
      <div class="config-table">
        <div class="config-row config-row-head lossy-row">
          <span><input type="checkbox" class="pick" id="lossy-pick-all" aria-label="Select every song shown" /></span>
          <span>Song</span><span>Album</span><span>Format</span><span>Status</span>
        </div>
        ${shown.map(row => `
          <label class="config-row lossy-row${lossy.picked.has(row.id) ? ' picked' : ''}">
            <span><input type="checkbox" class="pick" data-lossy-id="${esc(row.id)}" ${lossy.picked.has(row.id) ? 'checked' : ''}
              ${isOpenJob(row.job) ? 'disabled' : ''} aria-label="Pick ${esc(row.title)}" /></span>
            <span class="key">${esc(row.title)}<span class="lossy-sub">${esc(row.artist)}${row.fromYouTube ? ' · from YouTube' : ''}</span></span>
            <span class="value">${esc(row.album ?? '')}</span>
            <span><span class="dl-badge mp3">${esc((row.suffix || '?').toUpperCase())}</span></span>
            <span>${lossyStatus(row)}${lossyFindButton(row)}</span>
          </label>`).join('')}
      </div>
      ${visible.length > shown.length
        ? `<button type="button" class="btn btn-ghost" id="lossy-more">Show ${Math.min(LOSSY_PAGE, visible.length - shown.length)} more of ${visible.length - shown.length}</button>`
        : ''}`;
    document.getElementById('lossy-more')?.addEventListener('click', () => { lossy.shown += LOSSY_PAGE; renderLossy(); });
  }

  // A song that started meanwhile is no longer something to pick.
  for (const row of lossy.rows) if (isOpenJob(row.job)) lossy.picked.delete(row.id);
  const n = lossy.picked.size;

  // Select all means every song the filters show, drawn or not, that is not already running.
  const pickable = visible.filter(row => !isOpenJob(row.job));
  const pickedShown = pickable.filter(row => lossy.picked.has(row.id)).length;
  const all = document.getElementById('lossy-pick-all');
  if (all) {
    all.checked = pickable.length > 0 && pickedShown === pickable.length;
    all.indeterminate = pickedShown > 0 && pickedShown < pickable.length;
    all.disabled = pickable.length === 0;
  }
  const selectAll = document.getElementById('lossy-all');
  selectAll.textContent = pickable.length ? `Select all ${pickable.length}` : 'Select all';
  selectAll.hidden = pickable.length === 0 || pickedShown === pickable.length;
  document.getElementById('lossy-none').hidden = n === 0;
  document.getElementById('lossy-count').textContent = n ? `${n} ${n === 1 ? 'song' : 'songs'} picked` : 'Nothing picked';
  const go = document.getElementById('lossy-go');
  go.textContent = n ? `Find higher quality for ${n} ${n === 1 ? 'song' : 'songs'}` : 'Find higher quality';
  go.disabled = !n || !!closed;
  document.getElementById('lossy-cancel').hidden = !(view?.jobs || []).some(job => ['queued', 'waiting'].includes(job.state));
  renderLossyResults();
  document.getElementById('lossy-clear').hidden = !(view?.jobs || []).some(job => job && !isOpenJob(job));
}

// Every finished job, newest first, with its proof. A song that became lossless leaves the list
// above, so this is where its upgrade can still be read.
function renderLossyResults() {
  const holder = document.getElementById('lossy-results');
  if (!holder) return;
  const done = (lossy.view?.jobs || []).filter(job => !isOpenJob(job))
    .sort((a, b) => Date.parse(b.updatedUtc) - Date.parse(a.updatedUtc));
  if (!done.length) { holder.innerHTML = ''; return; }
  const took = s => typeof s === 'number' ? (s >= 90 ? `${Math.floor(s / 60)} min ${Math.round(s % 60)} s` : `${Math.round(s)} s`) : '';
  holder.className = 'lossy-results';
  holder.innerHTML = `<h4>Results</h4>${done.slice(0, 50).map(job => {
    const r = job.result;
    const facts = [];
    if (r?.before || r?.after) facts.push(['Changed', `${esc(r.before ?? '?')}${r.beforeBytes ? `, ${mb(r.beforeBytes)}` : ''} → ${esc(r.after ?? '?')}${r.afterBytes ? `, ${mb(r.afterBytes)}` : ''}`]);
    if (r?.newFile) facts.push(['New file', esc(r.newFile)]);
    if (r?.checks?.length) facts.push(['Passed', esc(r.checks.join(', '))]);
    if (r?.keptAt) facts.push(['Original kept in', esc(r.keptAt)]);
    if (r?.seconds != null) facts.push(['Took', took(r.seconds)]);
    if (!r && job.detail) facts.push(['Why', esc(job.detail)]);
    return `<div class="lossy-result">
      <div class="lossy-result-head"><strong>${esc(job.title ?? job.id)} <span class="lossy-sub" style="display:inline">${esc(job.artist ?? '')}</span></strong>
        <span class="dl-badge ${upgradeTone[job.state] ?? 'state'}">${esc(upgradeWords[job.state] ?? job.state)}</span>${lossyFindButton(job)}</div>
      ${facts.length ? `<dl>${facts.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join('')}</dl>` : ''}
    </div>`;
  }).join('')}`;
}

document.getElementById('lossy-list')?.addEventListener('change', event => {
  if (event.target.id === 'lossy-pick-all') {
    const pickable = lossyVisible().filter(row => !isOpenJob(row.job));
    for (const row of pickable) {
      if (event.target.checked) lossy.picked.add(row.id); else lossy.picked.delete(row.id);
    }
    renderLossy();
    return;
  }
  const box = event.target.closest('[data-lossy-id]');
  if (!box) return;
  if (box.checked) lossy.picked.add(box.dataset.lossyId); else lossy.picked.delete(box.dataset.lossyId);
  renderLossy();
});
for (const id of ['lossy-q', 'lossy-format', 'lossy-youtube', 'lossy-hide-tried']) {
  document.getElementById(id)?.addEventListener(id === 'lossy-q' ? 'input' : 'change', () => { lossy.shown = LOSSY_PAGE; renderLossy(); });
}
document.getElementById('lossy-refresh')?.addEventListener('click', () => loadLossy(true));
// Every song the filters show, drawn or not.
document.getElementById('lossy-all')?.addEventListener('click', () => {
  for (const row of lossyVisible()) if (!isOpenJob(row.job)) lossy.picked.add(row.id);
  renderLossy();
});
document.getElementById('lossy-none')?.addEventListener('click', () => { lossy.picked.clear(); renderLossy(); });

document.getElementById('lossy-go')?.addEventListener('click', async () => {
  const songs = lossy.rows.filter(row => lossy.picked.has(row.id));
  if (!songs.length) return;
  const n = songs.length;
  if (!(await askConfirm(`Find higher quality for ${n} ${n === 1 ? 'song' : 'songs'}?`, `Octo looks for a lossless copy of each on ${lossy.view?.source ?? 'the upgrade source'}. Each original is kept in quarantine until its replacement passes the checks.`, 'Find higher quality'))) return;
  const response = await lossyFetch('/api/admin/upgrades', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      songs: songs.map(row => ({
        navidromeId: row.id, title: row.title, artist: row.artist, album: row.album, suffix: row.suffix, attemptKey: row.attemptKey,
      })),
    }),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { toast(body.error || `HTTP ${response.status}`, 'error'); return; }
  toast(`Looking for higher quality for ${body.queued} ${body.queued === 1 ? 'song' : 'songs'}.${body.refused ? ` ${body.refused}` : ''}`);
  lossy.picked.clear();
  await loadUpgrades();
});

document.getElementById('lossy-cancel')?.addEventListener('click', async () => {
  const ids = (lossy.view?.jobs || []).filter(job => ['queued', 'waiting'].includes(job.state)).map(job => job.id);
  if (!ids.length) return;
  const response = await lossyFetch('/api/admin/upgrades/cancel', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ ids }),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { toast(body.error || `HTTP ${response.status}`, 'error'); return; }
  toast(`Took back ${body.cancelled} ${body.cancelled === 1 ? 'song' : 'songs'}.`);
  for (const row of lossy.rows) if (ids.includes(row.id)) row.job = null;
  await loadUpgrades();
});

document.getElementById('lossy-clear')?.addEventListener('click', async () => {
  const response = await lossyFetch('/api/admin/upgrades/clear', { method: 'POST' });
  if (!response.ok) { toast(`HTTP ${response.status}`, 'error'); return; }
  for (const row of lossy.rows) if (row.job && !isOpenJob(row.job)) row.job = null;
  await loadUpgrades();
});

// ────────────────────────────────────────────────────────────────
// Updates: whether a newer release is out, and Update now through the host helper
// ────────────────────────────────────────────────────────────────
// Octo asks GitHub for its releases; the host helper (scripts/updater) does the update when
// asked through a file in the config folder. Without the helper this card shows the command.
let updateInfo = null;
let updateRequestId = null;
let updatePollTimer = null;
let updateOutageSince = null;
const UPDATE_RUNNING = ['accepted', 'fetching', 'building', 'restarting'];
const UPDATE_STEPS = [
  ['accepted', 'Handed to the update helper'],
  ['fetching', 'Fetching the release'],
  ['building', 'Building the new Octo'],
  ['restarting', 'Restarting Octo'],
];
const UPDATE_LATER_KEY = 'octo.update.later';

function updateLater() {
  try { return localStorage.getItem(UPDATE_LATER_KEY); } catch { return null; }
}

// Release notes are Markdown from GitHub. Everything is escaped first; then only headings,
// lists, bold, inline code, rules and links to github.com come back as markup.
function renderNotes(markdown) {
  const text = String(markdown ?? '').replace(/<!--[\s\S]*?-->/g, '').replace(/\r/g, '');
  const inline = s => escapeHtml(s)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\[([^\]]+)\]\((https:\/\/github\.com\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>')
    // A link anywhere else keeps its words and loses the address.
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1');
  const out = [];
  let para = [];
  let list = null;
  const flushPara = () => { if (para.length) out.push(`<p>${inline(para.join(' '))}</p>`); para = []; };
  const flushList = () => {
    if (list) out.push(`<${list.tag}>${list.items.map(item => `<li>${inline(item)}</li>`).join('')}</${list.tag}>`);
    list = null;
  };
  for (const raw of text.split('\n')) {
    const line = raw.trimEnd();
    let m;
    if (!line.trim()) { flushPara(); flushList(); continue; }
    if ((m = line.match(/^#{1,6}\s+(.*)$/))) { flushPara(); flushList(); out.push(`<h4>${inline(m[1])}</h4>`); continue; }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { flushPara(); flushList(); out.push('<hr>'); continue; }
    const bullet = line.match(/^\s*[-*+]\s+(.*)$/);
    const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (bullet || numbered) {
      flushPara();
      const tag = bullet ? 'ul' : 'ol';
      if (list && list.tag !== tag && !/^\s/.test(line)) flushList();
      if (!list) list = { tag, items: [] };
      list.items.push((bullet || numbered)[1]);
      continue;
    }
    // An indented line under a list item continues it.
    if (list && /^\s/.test(line)) { list.items[list.items.length - 1] += ` ${line.trim()}`; continue; }
    flushList();
    para.push(line.trim());
  }
  flushPara();
  flushList();
  return out.join('');
}

function showReleaseNotes() {
  const info = updateInfo;
  const releases = info?.newer?.length ? info.newer : (info?.latest ? [info.latest] : []);
  if (!releases.length) return;
  const modal = document.getElementById('notes-modal');
  document.getElementById('notes-title').textContent = releases.length > 1
    ? `What's new in the last ${releases.length} releases` : `What's new in ${releases[0].tag}`;
  document.getElementById('notes-body').innerHTML = releases.map(release => {
    const when = release.publishedUtc ? new Date(release.publishedUtc).toLocaleDateString() : '';
    const notes = renderNotes(release.notes) || '<p>This release has no notes.</p>';
    return `<section class="notes-release"><h3>${escapeHtml(release.name || release.tag)}${when ? ` <span class="notes-date">${escapeHtml(when)}</span>` : ''}</h3>${notes}</section>`;
  }).join('');
  const link = document.getElementById('notes-link');
  const url = releases[0].url || '';
  link.hidden = !url.startsWith('https://github.com/');
  if (!link.hidden) link.href = url;
  const opener = document.activeElement;
  const app = document.querySelector('.app');
  if (app) app.inert = true;
  modal.hidden = false;
  const closeBtn = document.getElementById('notes-close');
  closeBtn.focus();
  const close = () => {
    modal.hidden = true;
    if (app) app.inert = false;
    closeBtn.removeEventListener('click', close);
    modal.removeEventListener('keydown', onKey);
    modal.removeEventListener('mousedown', onBackdrop);
    if (opener && typeof opener.focus === 'function') opener.focus();
  };
  const onKey = e => {
    if (e.key === 'Escape') { e.preventDefault(); close(); return; }
    if (e.key === 'Tab') {
      const stops = Array.from(modal.querySelectorAll('a[href], button')).filter(el => !el.hidden && el.offsetParent !== null);
      const head = stops[0];
      const tail = stops[stops.length - 1];
      if (e.shiftKey && document.activeElement === head) { e.preventDefault(); tail.focus(); }
      else if (!e.shiftKey && document.activeElement === tail) { e.preventDefault(); head.focus(); }
    }
  };
  const onBackdrop = e => { if (e.target === modal) close(); };
  closeBtn.addEventListener('click', close);
  modal.addEventListener('keydown', onKey);
  modal.addEventListener('mousedown', onBackdrop);
}

function updateRunActive(info = updateInfo) {
  return !!info && (info.pending || (!!info.run && UPDATE_RUNNING.includes(info.run.state)));
}

function renderUpdate() {
  const info = updateInfo;
  if (!info) return;
  const byId = id => document.getElementById(id);
  const latest = info.latest;
  const helper = info.helper?.installed;
  const active = updateRunActive();
  const run = info.run;
  const ours = !!run && !!updateRequestId && run.id === updateRequestId;

  let line;
  if (!info.enabled) line = 'Looking for new releases is off.';
  else if (active) line = `Updating to ${run?.tag || latest?.tag || 'the newest release'}…`;
  else if (info.updateAvailable) {
    const behind = info.newer.length > 1 ? ` That is ${info.newer.length} releases ahead.` : '';
    line = `Octo ${latest.tag} is out. You run ${info.running}.${behind}`;
  } else if (latest) {
    const checked = info.checkedUtc ? `, checked ${relTime(info.checkedUtc)}` : '';
    line = info.standing === 'ahead'
      ? `Up to date: this build is newer than the newest release, ${latest.tag}${checked}.`
      : info.standing === 'current' ? `Up to date${checked}.`
        : `The newest release is ${latest.tag}${checked}. This build names no release, so Octo cannot compare.`;
  } else line = info.error ? '' : 'Not checked yet.';
  if (info.enabled && info.error) line = `${line} ${info.error}`.trim();
  byId('update-line').textContent = line;

  byId('update-now').hidden = !(info.updateAvailable && helper && !active);
  byId('update-notes').hidden = !latest;
  byId('update-check').hidden = !info.enabled;
  byId('update-check').disabled = active;

  // The steps, while a run is under way or just after one this page started.
  const steps = byId('update-steps');
  const log = byId('update-log');
  const showSteps = active || ours;
  steps.hidden = !showSteps;
  log.hidden = true;
  if (showSteps) {
    const state = updateOutageSince ? 'restarting' : (info.pending ? 'accepted' : run?.state);
    const at = UPDATE_STEPS.findIndex(([key]) => key === state);
    if (state === 'failed') {
      steps.innerHTML = `<li class="is-failed">${escapeHtml(run.error || 'The update failed.')}</li>`;
      if (run.log?.length) { log.textContent = run.log.join('\n'); log.hidden = false; }
    } else if (state === 'done') {
      steps.innerHTML = `<li class="is-done">${escapeHtml(run.step || `Octo now runs ${run.tag}`)}</li>`;
    } else {
      steps.innerHTML = UPDATE_STEPS.map(([key, label], i) => {
        const cls = at < 0 ? '' : i < at ? 'is-done' : i === at ? 'is-current' : '';
        const words = i === at && run?.step && !info.pending && !updateOutageSince ? run.step : label;
        return `<li class="${cls}">${escapeHtml(words)}</li>`;
      }).join('');
    }
  } else if (run && run.state === 'failed') {
    // A failed run is worth seeing, even on a page that did not start it.
    steps.hidden = false;
    steps.innerHTML = `<li class="is-failed">The last update, to ${escapeHtml(run.tag || 'a release')}, failed: ${escapeHtml(run.error || 'no reason given')}</li>`;
    if (run.log?.length) { log.textContent = run.log.join('\n'); log.hidden = false; }
  }

  // The command, when there is no helper or it did not answer.
  const manual = byId('update-manual');
  const unanswered = !!info.unanswered && info.unanswered === updateRequestId;
  manual.hidden = !(info.updateAvailable && !active && (!helper || unanswered));
  if (!manual.hidden) {
    const where = info.helper?.dir ? `in ${info.helper.dir}` : 'in your Octo folder';
    byId('update-manual-where').textContent = unanswered
      ? `The update helper on this server's host did not answer, so nothing changed. To update by hand, run this ${where}:`
      : `To update, run this ${where}:`;
    const image = info.helper?.mode === 'image';
    byId('update-command').textContent = image ? info.imageCommand : info.command;
    byId('update-image-line').hidden = image;
    byId('update-image-command').textContent = info.imageCommand;
    byId('update-helper-hint').hidden = !!helper;
  }

  // The banner on every page, and the dot on About.
  const banner = byId('update-banner');
  banner.hidden = !(info.updateAvailable && !active && updateLater() !== latest?.tag);
  if (!banner.hidden) byId('update-banner-text').textContent = `Octo ${latest.tag} is out. You run ${info.running}.`;
  const about = document.querySelector('.sidebar-nav-item[data-tab="about"]');
  let flag = about?.querySelector('.nav-flag');
  if (about && info.updateAvailable && !flag) {
    flag = document.createElement('span');
    flag.className = 'nav-flag nav-flag-update';
    flag.innerHTML = '<span class="visually-hidden"> (update available)</span>';
    about.appendChild(flag);
  } else if (flag && !info.updateAvailable) flag.remove();

  if (active && !updatePollTimer) pollUpdate();
}

async function loadUpdate({ check = false } = {}) {
  try {
    const response = check
      ? await api('/api/admin/update/check', { method: 'POST' })
      : await api('/api/admin/update', { cache: 'no-store' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    updateInfo = await response.json();
  } catch {
    if (!updateInfo) document.getElementById('update-line').textContent = "Couldn't ask Octo about new releases.";
    return;
  }
  renderUpdate();
}

// Every 2 seconds while a run is under way. Octo is down while it restarts, so failed polls
// read as "Restarting" for up to 15 minutes before the page gives up.
function pollUpdate() {
  clearTimeout(updatePollTimer);
  updatePollTimer = setTimeout(async () => {
    const before = updateInfo?.running;
    try {
      const response = await api('/api/admin/update', { cache: 'no-store' });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      updateInfo = await response.json();
      updateOutageSince = null;
    } catch {
      updateOutageSince ??= Date.now();
      if (Date.now() - updateOutageSince > 15 * 60_000) {
        updatePollTimer = null;
        updateOutageSince = null;
        document.getElementById('update-line').textContent = 'Octo has not come back after the update. "docker compose logs octo" on the host says why.';
        return;
      }
      renderUpdate();
      pollUpdate();
      return;
    }
    updatePollTimer = null;
    renderUpdate();
    if (updateRunActive()) { pollUpdate(); return; }
    const run = updateInfo.run;
    if (run?.state === 'done' && before && updateInfo.running !== before) {
      toast(`Octo now runs ${updateInfo.running}.`, 'ok');
      await loadSettings();
    } else if (run?.state === 'failed' && run.id === updateRequestId) {
      toast('The update failed. Octo still runs the old version.', 'error');
    } else if (updateRequestId && updateInfo.unanswered === updateRequestId) {
      toast('The update helper did not answer. The command to run is on the About page.', 'error');
    }
  }, 2000);
}

document.getElementById('update-check')?.addEventListener('click', async event => {
  const btn = event.currentTarget;
  btn.disabled = true;
  await loadUpdate({ check: true });
  btn.disabled = updateRunActive();
});

document.getElementById('update-now')?.addEventListener('click', async () => {
  const tag = updateInfo?.latest?.tag;
  if (!tag) return;
  const unsavedCards = document.querySelectorAll('form.unsaved').length;
  const unsavedNote = unsavedCards
    ? `\n\nYou have unsaved changes on ${unsavedCards} card${unsavedCards === 1 ? '' : 's'}; the restart discards them.`
    : '';
  if (!(await askConfirm(`Update to ${tag}?`,
    `Octo fetches ${tag}, builds it, and restarts. Playback through Octo stops for a minute or two. If the build fails, nothing changes.${unsavedNote}`,
    'Update now'))) return;
  const response = await api('/api/admin/update', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ tag }),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { toast(body.error || `HTTP ${response.status}`, 'error'); await loadUpdate(); return; }
  updateRequestId = body.id;
  updateInfo = { ...updateInfo, pending: true, pendingId: body.id };
  renderUpdate();
});

document.getElementById('update-notes')?.addEventListener('click', showReleaseNotes);
document.getElementById('update-banner-notes')?.addEventListener('click', showReleaseNotes);
document.getElementById('update-banner-later')?.addEventListener('click', () => {
  try { localStorage.setItem(UPDATE_LATER_KEY, updateInfo?.latest?.tag ?? ''); } catch { /* the banner comes back next visit */ }
  document.getElementById('update-banner').hidden = true;
});
document.getElementById('update-copy')?.addEventListener('click', async () => {
  const text = document.getElementById('update-command').textContent;
  try { await navigator.clipboard.writeText(text); toast('Command copied.', 'ok'); }
  catch { await askDialog({ title: 'Copy the command', confirm: 'Done', cancel: null, input: { value: text, readOnly: true } }); }
});

// ────────────────────────────────────────────────────────────────
// Soulseek: Open slskd, and the Sharing card
// ────────────────────────────────────────────────────────────────
// The browser only ever gets slskd's address. slskd asks for its own sign-in, so the login Octo
// uses for slskd stays on the server.
function safeHttpUrl(value) {
  try {
    const url = new URL(String(value ?? '').trim());
    return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : null;
  } catch { return null; }
}

function slskdWebUrl() {
  const own = safeHttpUrl(currentSettings?.Soulseek?.WebUrl);
  if (own) return own;
  const base = safeHttpUrl(currentSettings?.Soulseek?.BaseUrl);
  const port = base ? (new URL(base).port || '5030') : '5030';
  if (base) {
    // An address or a dotted name is somewhere this browser can go too. "slskd" is a name only
    // Docker's network knows, and localhost would be this browser's own machine.
    const host = new URL(base).hostname;
    const local = host === 'localhost' || host.startsWith('127.') || host === '[::1]';
    if (!local && (host.includes('.') || host.startsWith('['))) return base;
  }
  return `http://${location.hostname}:${port}/`;
}

function renderSlskdOpen() {
  const open = document.getElementById('slskd-open');
  if (open) open.href = slskdWebUrl();
}

const shareSize = bytes => {
  const units = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes, unit = 0;
  while (value >= 1000 && unit < units.length - 1) { value /= 1000; unit++; }
  return `${unit === 0 ? value : value.toFixed(value >= 100 ? 0 : 1)} ${units[unit]}`;
};
const sharePlural = (n, one, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
// "day", "3 hours", "90 minutes": the plainest whole unit.
const shareEvery = minutes => {
  const [n, unit] = minutes % 1440 === 0 ? [minutes / 1440, 'day']
    : minutes % 60 === 0 ? [minutes / 60, 'hour'] : [minutes, 'minute'];
  return n === 1 ? unit : sharePlural(n, unit);
};

function sharePortText(port) {
  const when = port?.checkedAt ? `, tested ${new Date(port.checkedAt).toLocaleString()}` : '';
  const number = port?.port ?? 'unknown';
  switch (port?.state) {
    case 'Open': return `${number}, open: other people can connect${when}`;
    case 'Closed': return `${number}, closed: other people cannot connect${when}`;
    case 'Off': return `${number}, not tested (the test is off below)`;
    default: return `${number}, ${port?.error ? `not known. ${port.error}` : 'not tested yet'}`;
  }
}

let shareScanTimer = null;

function renderSharing(report) {
  const line = document.getElementById('share-line');
  const facts = document.getElementById('share-facts');
  const warnings = document.getElementById('share-warnings');
  if (!line || !facts || !warnings) return;

  const shared = (report.folders || []).filter(folder => !folder.excluded);
  const choice = report.switch || {};
  const toggle = document.getElementById('share-switch');
  if (toggle && !toggle.dataset.busy) {
    toggle.checked = !!choice.on;
    toggle.disabled = !report.reachable;
  }
  const outside = choice.control === 'Outside' ? ' slskd\'s shares are set outside Octo, so the switch above stands aside.' : '';
  if (!report.reachable) line.textContent = 'Octo cannot reach slskd right now.';
  else if (shared.length === 0 && !choice.on && choice.control !== 'Outside')
    line.textContent = 'Sharing is off, so nothing in your library is shared. Many people on Soulseek send files more readily to someone who shares, so turning it on can help your own downloads.';
  else if (shared.length === 0) line.textContent = `You share nothing yet.${outside}`;
  else {
    let text = report.files == null ? 'Sharing.'
      : `Sharing ${sharePlural(report.files, 'file')} in ${sharePlural(report.directories ?? 0, 'folder')}.`;
    if (report.scanning) text += ` slskd is looking through your folders now${report.scanProgress != null ? ` (${Math.round(report.scanProgress * 100)}%)` : ''}.`;
    if (report.networkFiles != null && report.files != null && report.networkFiles !== report.files && !report.scanning)
      text += ` Soulseek has ${sharePlural(report.networkFiles, 'file')} on record for you.`;
    line.textContent = text + outside;
  }

  const rows = [];
  if (report.reachable) {
    const folderText = folder => `${folder.alias || folder.path}${folder.alias && folder.path ? ` (${folder.path})` : ''}${folder.files != null ? `, ${sharePlural(folder.files, 'file')}` : ''}`;
    rows.push(['Shared', shared.length ? shared.map(folderText).join('; ') : 'nothing']);
    const kept = (report.folders || []).filter(folder => folder.excluded);
    if (kept.length) rows.push(['Kept out', kept.map(folder => folder.path).join('; ')]);
    const up = report.uploads;
    rows.push(['Uploading now', `${up.sending.toLocaleString()} sending, ${up.waiting.toLocaleString()} waiting`]);
    rows.push(['Last 7 days', up.files
      ? `${sharePlural(up.files, 'file')}, ${shareSize(up.bytes)}, to ${sharePlural(up.people, 'person', 'people')}${up.failed ? `; ${up.failed.toLocaleString()} failed` : ''}`
      : `nothing downloaded from you${up.failed ? `; ${up.failed.toLocaleString()} failed` : ''}`]);
    if (up.lastUploadAt) rows.push(['Last upload', new Date(up.lastUploadAt).toLocaleString()]);
    const slots = report.uploadSlots != null ? `${report.uploadSlots.toLocaleString()} at a time` : 'slskd did not say';
    const speed = report.uploadSpeedLimitKiB != null ? `${report.uploadSpeedLimitKiB.toLocaleString()} KiB/s in all` : 'no speed limit';
    rows.push(['Upload limits', `${slots}, ${speed}`]);
    rows.push(['Rescans', report.rescanMinutes
      ? `every ${shareEvery(report.rescanMinutes)}`
      : 'when slskd starts, or when you press Rescan']);
    rows.push(['Listening port', sharePortText(report.port)]);
  }
  facts.innerHTML = rows.map(([term, value]) => `<div><dt>${esc(term)}</dt><dd>${esc(value)}</dd></div>`).join('');
  facts.hidden = rows.length === 0;

  const list = report.warnings || [];
  warnings.innerHTML = list.map(warning => `<div class="notice notice-warn" role="status">${esc(warning.text)}</div>`).join('');
  warnings.hidden = list.length === 0;

  document.getElementById('share-rescan').disabled = !report.reachable || shared.length === 0 || report.scanning;

  // While slskd looks through the folders, look again every few seconds until the count settles.
  clearTimeout(shareScanTimer);
  if (report.scanning && document.querySelector('[data-pane="soulseek"].active'))
    shareScanTimer = setTimeout(() => loadSharing(), 4000);
  document.getElementById('share-test-port').disabled = !report.reachable || report.port?.state === 'Off' || report.listenPort == null;
}

async function loadSharing({ testPort = false } = {}) {
  const line = document.getElementById('share-line');
  if (!line) return null;
  try {
    const r = testPort
      ? await api('/api/admin/soulseek/sharing/test-port', { method: 'POST' })
      : await api('/api/admin/soulseek/sharing', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const report = await r.json();
    renderSharing(report);
    return report;
  } catch (e) {
    line.textContent = `Octo did not answer: ${e.message}`;
    return null;
  }
}

document.getElementById('share-refresh')?.addEventListener('click', () => loadSharing());
document.getElementById('share-switch')?.addEventListener('change', async event => {
  const toggle = event.currentTarget;
  const on = toggle.checked;
  toggle.dataset.busy = '1';
  toggle.disabled = true;
  note(document.getElementById('share-line'), on ? 'Turning sharing on…' : 'Turning sharing off…', 'busy');
  try {
    const r = await api('/api/admin/soulseek/sharing/share', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ on }),
    });
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(body.error || `HTTP ${r.status}`);
    delete toggle.dataset.busy;
    renderSharing(body);
    const problem = body.switch?.problem;
    if (problem) note(document.getElementById('share-line'), 'Saved, but slskd did not take it. See the note below.', 'error');
    else note(document.getElementById('share-line'), on ? 'Sharing is on. slskd is looking through your library.' : 'Sharing is off. Nothing is shared.', 'ok');
  } catch (e) {
    delete toggle.dataset.busy;
    toggle.checked = !on;
    toggle.disabled = false;
    note(document.getElementById('share-line'), e.message || 'Octo did not answer.', 'error');
  }
});
document.getElementById('share-test-port')?.addEventListener('click', async event => {
  const button = event.currentTarget;
  button.disabled = true;
  note(button, 'Asking Soulseek\'s port test…', 'busy');
  const report = await loadSharing({ testPort: true });
  const state = report?.port?.state;
  if (state === 'Open') note(button, 'Open. Other people can connect to slskd.', 'ok');
  else if (state === 'Closed') note(button, 'Closed. See the note below.', 'error');
  else note(button, report?.port?.error || 'No answer from the port test.', 'info');
});
document.getElementById('share-rescan')?.addEventListener('click', async event => {
  const button = event.currentTarget;
  button.disabled = true;
  const r = await api('/api/admin/soulseek/sharing/rescan', { method: 'POST' }).catch(() => null);
  const body = await r?.json().catch(() => ({}));
  if (r?.ok) note(button, 'slskd is looking through your shared folders. A big library takes a while.', 'ok');
  else note(button, body?.error || 'Octo did not answer.', 'error');
  setTimeout(() => loadSharing(), 2000);
});

// ────────────────────────────────────────────────────────────────
// Spotify import: lists from a Spotify account, a link or a file, what the library has of each,
// playlists kept in Navidrome, and the trickle that fetches the rest
// ────────────────────────────────────────────────────────────────
const imp = { view: null, user: null, open: null, detail: null, picked: new Set(), timer: null, waitingForPaste: false };
const impEl = id => document.getElementById(id);
const impStateWords = {
  have: 'In your library', missing: 'Missing', queued: 'Queued', downloading: 'Fetching',
  done: 'Fetched', notFound: 'Not found', skipped: 'Skipped',
};
const impStateTone = { have: 'good', done: 'good', missing: 'state', queued: 'state', downloading: 'state', notFound: 'warn', skipped: 'warn' };
const impSourceWords = { spotifyLiked: 'Spotify', spotifyPlaylist: 'Spotify', link: 'Link', file: 'File' };

// The page acts for the Navidrome account signed in on it, like Better quality.
async function impFetch(path, options = {}) {
  let response = await api(path, { credentials: 'same-origin', ...options });
  if (response.status === 401 && await browseAuthenticate(impEl('imp-lists'))) {
    response = await api(path, { credentials: 'same-origin', ...options });
  }
  return response;
}

async function impPost(path, body, anchor) {
  const response = await impFetch(path, {
    method: 'POST',
    headers: body instanceof FormData ? {} : { 'Content-Type': 'application/json' },
    body: body instanceof FormData ? body : JSON.stringify(body ?? {}),
  });
  const answer = await response.json().catch(() => ({}));
  const message = answer.message || answer.error || `HTTP ${response.status}`;
  if (anchor) note(anchor, message, response.ok ? 'ok' : 'error');
  return { ok: response.ok, answer };
}

async function loadImports() {
  clearTimeout(imp.timer);
  if (!imp.view) impEl('imp-lists').innerHTML = stateBlock('loading', 'Reading your lists…');
  try {
    const response = await impFetch('/api/admin/imports');
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
    imp.view = body.overview;
    imp.user = body.user;
    renderImports();
    if (imp.open) await loadImportDetail(imp.open, false);
  } catch (error) {
    impEl('imp-lists').innerHTML = stateBlock('error', error.message);
  }
  // Live while something is moving and the page is open.
  const moving = imp.view && (imp.view.reading?.busy || imp.view.trickle?.queued > 0 || imp.view.trickle?.downloading > 0);
  if (moving && document.querySelector('section[data-pane="imports"].active')) imp.timer = setTimeout(loadImports, 3000);
}

function impWhen(utc) {
  if (!utc) return '';
  const date = new Date(utc);
  return date.toDateString() === new Date().toDateString()
    ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : date.toLocaleDateString();
}

function renderImports() {
  const view = imp.view;
  if (!view) return;
  const spotify = view.spotify;

  const problem = impEl('imp-library-problem');
  problem.hidden = !view.libraryProblem;
  if (view.libraryProblem) problem.textContent = view.libraryProblem;

  // The account.
  impEl('imp-redirect-shown').textContent = spotify.redirectUri;
  impEl('imp-account-title').textContent = spotify.connected ? `Connected as ${spotify.account}`
    : spotify.problem ? 'Sign in again' : 'Not connected';
  let line;
  if (!spotify.configured) line = 'Add your Spotify app\'s Client ID below first. The steps are under "Set up your Spotify app".';
  else if (spotify.redirectProblem) line = `The redirect URI will not work: ${spotify.redirectProblem}`;
  else if (spotify.problem) line = spotify.problem;
  else if (spotify.connected) line = `Spotify ends this sign-in on ${new Date(spotify.endsUtc).toLocaleDateString()}, six months after it was made; connect again then.`;
  else line = 'Connect opens Spotify in a new tab to allow Octo to read your library.';
  impEl('imp-account-line').textContent = line;
  impEl('imp-connect').textContent = spotify.connected ? 'Connect again' : 'Connect Spotify';
  impEl('imp-connect').disabled = !spotify.configured || !!spotify.redirectProblem;
  impEl('imp-connect').classList.toggle('btn-primary', !spotify.connected);
  impEl('imp-read').hidden = !spotify.connected;
  impEl('imp-read').disabled = !!view.reading?.busy;
  impEl('imp-disconnect').hidden = !spotify.connected && !spotify.problem;
  impEl('imp-paste-row').hidden = !imp.waitingForPaste || spotify.octoFinishes;
  impEl('imp-guide').open = !spotify.configured;

  const reading = impEl('imp-reading');
  const read = view.reading;
  reading.hidden = !read?.busy && !read?.error;
  reading.classList.toggle('imp-error', !!read?.error);
  reading.textContent = read?.busy ? `${read.step}…` : read?.error ?? '';

  renderImportLists();
  renderTrickle();
}

function renderImportLists() {
  const lists = imp.view.lists || [];
  const holder = impEl('imp-lists');
  if (!lists.length) {
    holder.innerHTML = stateBlock('empty', imp.view.spotify.connected
      ? 'No lists yet. Read again to look at your Spotify once more.'
      : 'No lists yet. Connect Spotify, or add a link or a file.');
    return;
  }
  holder.innerHTML = `
    <div class="config-table">
      <div class="config-row config-row-head imp-row">
        <span>List</span><span>In your library</span><span>Keep as playlist</span><span>Get missing songs</span><span></span>
      </div>
      ${lists.map(list => {
        const total = list.total || list.have + list.missing + list.queued + list.downloading + list.done + list.notFound + list.skipped;
        const pct = total ? Math.round(list.have / total * 100) : 0;
        const coming = list.queued + list.downloading;
        const sub = [
          impSourceWords[list.source] ?? list.source,
          list.by ? `by ${list.by}` : null,
          list.gone ? 'no longer on your Spotify' : null,
          `read ${impWhen(list.readUtc)}`,
        ].filter(Boolean).join(' · ');
        const counts = [`${list.have} of ${total}`, list.missing ? `${list.missing} missing` : null,
          coming ? `${coming} coming` : null, list.notFound ? `${list.notFound} not found` : null].filter(Boolean).join(' · ');
        return `
          <div class="config-row imp-row${imp.open === list.id ? ' picked' : ''}">
            <span class="key"><button type="button" class="link-btn imp-name" data-imp-open="${esc(list.id)}">${esc(list.name)}</button>
              <span class="lossy-sub">${esc(sub)}</span>
              ${list.partial ? `<span class="lossy-detail">${esc(list.partial)}</span>` : ''}
              ${list.playlistNote ? `<span class="lossy-detail">${esc(list.playlistNote)}</span>` : ''}</span>
            <span><span class="lossy-sub">${esc(counts)}</span><span class="lossy-meter"><span style="width:${pct}%"></span></span></span>
            <span class="imp-switch-cell"><span class="imp-cell-label">Keep as playlist</span><label class="switch" title="Keep as playlist"><input type="checkbox" data-imp-playlist="${esc(list.id)}" ${list.keepPlaylist ? 'checked' : ''}
              aria-label="Keep ${esc(list.name)} as a Navidrome playlist" /><span class="sw-track"></span><span class="sw-thumb"></span></label></span>
            <span class="imp-switch-cell"><span class="imp-cell-label">Get missing songs</span><label class="switch" title="Get missing songs"><input type="checkbox" data-imp-fetch="${esc(list.id)}" ${list.getMissing ? 'checked' : ''}
              aria-label="Get the songs ${esc(list.name)} is missing" /><span class="sw-track"></span><span class="sw-thumb"></span></label></span>
            <span class="imp-row-actions">
              ${list.canRefresh ? `<button type="button" class="btn btn-ghost" data-imp-refresh="${esc(list.id)}">Read again</button>` : ''}
              <button type="button" class="btn btn-ghost" data-imp-remove="${esc(list.id)}" aria-label="Remove ${esc(list.name)}">${icon('i-trash')}</button>
            </span>
          </div>`;
      }).join('')}
    </div>`;
}

async function loadImportDetail(id, scroll = true) {
  imp.open = id;
  const box = impEl('imp-detail');
  box.hidden = false;
  if (!imp.detail || imp.detail.list.id !== id) {
    imp.picked.clear();
    box.innerHTML = stateBlock('loading', 'Reading the list…');
  }
  const response = await impFetch(`/api/admin/imports/lists/${encodeURIComponent(id)}`);
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { box.innerHTML = stateBlock('error', body.error || `HTTP ${response.status}`); return; }
  imp.detail = body;
  renderImportDetail();
  renderImportLists();
  if (scroll) box.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
}

function renderImportDetail() {
  const { list, tracks } = imp.detail;
  const box = impEl('imp-detail');
  const pickable = tracks.filter(t => ['missing', 'notFound', 'skipped'].includes(t.state));
  for (const key of [...imp.picked]) if (!pickable.some(t => t.key === key)) imp.picked.delete(key);
  const n = imp.picked.size;
  const length = s => (s ? `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}` : '');
  box.innerHTML = `
    <div class="imp-detail-head">
      <h4>${esc(list.name)}</h4>
      <span class="lossy-sub">${list.have} in your library, ${tracks.length - list.have} not</span>
      <button type="button" class="btn btn-ghost" id="imp-detail-close">Close</button>
    </div>
    <div class="config-table imp-tracks">
      <div class="config-row config-row-head imp-track">
        <span></span><span>Song</span><span>Album</span><span>Length</span><span>Status</span>
      </div>
      ${tracks.map(track => {
        const can = ['missing', 'notFound', 'skipped'].includes(track.state);
        const pct = typeof track.progress === 'number' ? Math.round(track.progress * 100) : null;
        return `
          <label class="config-row imp-track${imp.picked.has(track.key) ? ' picked' : ''}">
            <span>${can ? `<input type="checkbox" class="pick" data-imp-pick="${esc(track.key)}" ${imp.picked.has(track.key) ? 'checked' : ''} aria-label="Pick ${esc(track.title)}" />` : ''}</span>
            <span class="key">${esc(track.title)}<span class="lossy-sub">${esc(track.artist)}</span></span>
            <span class="value">${esc(track.album ?? '')}</span>
            <span class="value">${esc(length(track.seconds))}</span>
            <span><span class="dl-badge ${impStateTone[track.state] ?? 'state'}">${esc(impStateWords[track.state] ?? track.state)}</span>
              ${track.detail ? `<span class="lossy-detail">${esc(track.detail)}${pct !== null ? ` ${pct}%` : ''}</span>` : ''}
              ${pct !== null ? `<span class="lossy-meter"><span style="width:${pct}%"></span></span>` : ''}</span>
          </label>`;
      }).join('')}
    </div>
    <div class="cover-bar">
      <span class="cover-count">${n ? `${n} ${n === 1 ? 'song' : 'songs'} picked` : 'Pick missing songs to get only those'}</span>
      <span class="cover-hint"></span>
      ${pickable.length ? `<button type="button" class="btn btn-ghost" id="imp-pick-all">Select all ${pickable.length}</button>` : ''}
      <button type="button" class="btn btn-primary" id="imp-get" ${n ? '' : 'disabled'}>Get ${n || ''} ${n === 1 ? 'song' : 'songs'}</button>
    </div>`;
}

function renderTrickle() {
  const t = imp.view.trickle;
  const words = {
    idle: 'Nothing to fetch',
    running: t.downloading ? 'Fetching' : 'Waiting for the next turn',
    paused: 'Paused',
    off: 'Off',
    yielding: 'Waiting for other downloads',
    waitingForSoulseek: 'Waiting for Soulseek',
  };
  impEl('imp-trickle-title').textContent = words[t.state] ?? t.state;
  const parts = [];
  if (t.state === 'off') parts.push('Songs an hour is 0, so nothing starts. Change it above.');
  else parts.push(`Up to ${t.perHour} ${t.perHour === 1 ? 'song' : 'songs'} an hour.`);
  if (t.state === 'running' && t.nextUtc && !t.downloading) parts.push(`Next song at ${impWhen(t.nextUtc)}.`);
  if (t.state === 'yielding') parts.push('Someone\'s own downloads come first; it goes on once they finish.');
  if (t.state === 'waitingForSoulseek') parts.push('slskd is not logged in to Soulseek; it goes on once it is back.');
  const tally = [[t.queued, 'waiting'], [t.downloading, 'fetching'], [t.done, 'fetched'], [t.notFound, 'not found'], [t.skipped, 'skipped']]
    .filter(([count]) => count > 0).map(([count, label]) => `${count} ${label}`);
  if (tally.length) parts.push(`${tally.join(', ')}.`);
  impEl('imp-trickle-line').textContent = parts.join(' ');

  const pause = impEl('imp-pause');
  pause.hidden = t.state === 'idle' && t.queued === 0;
  pause.textContent = t.state === 'paused' ? 'Go on' : 'Pause';
  impEl('imp-retry').hidden = !t.notFound;
  impEl('imp-clear').hidden = !(t.done + t.notFound + t.skipped);

  const row = (track, action = '') => {
    const pct = typeof track.progress === 'number' ? Math.round(track.progress * 100) : null;
    return `
      <div class="config-row imp-trickle-row">
        <span class="key">${esc(track.title)}<span class="lossy-sub">${esc(track.artist)}</span></span>
        <span><span class="dl-badge ${impStateTone[track.state] ?? 'state'}">${esc(impStateWords[track.state] ?? track.state)}</span>
          ${track.detail ? `<span class="lossy-detail">${esc(track.detail)}${pct !== null ? ` ${pct}%` : ''}</span>` : ''}
          ${pct !== null ? `<span class="lossy-meter"><span style="width:${pct}%"></span></span>` : ''}</span>
        <span class="imp-row-actions">${action}</span>
      </div>`;
  };
  const sections = [];
  if (t.current) sections.push(`<h4 class="imp-subhead">Now</h4><div class="config-table">${row(t.current)}</div>`);
  if (t.next?.length) {
    sections.push(`<h4 class="imp-subhead">Next</h4><div class="config-table">${t.next.map(track =>
      row(track, `<button type="button" class="btn btn-ghost" data-imp-skip="${esc(track.key)}">Skip</button>`)).join('')}</div>`);
  }
  if (t.recent?.length) {
    sections.push(`<h4 class="imp-subhead">Lately</h4><div class="config-table">${t.recent.map(track =>
      row(track, ['notFound', 'skipped'].includes(track.state)
        ? `<button type="button" class="btn btn-ghost" data-imp-retry="${esc(track.key)}">Try again</button>` : '')).join('')}</div>`);
  }
  impEl('imp-trickle-rows').innerHTML = sections.join('');
}

impEl('imp-connect')?.addEventListener('click', async event => {
  // Opened before the answer, inside the click, so the browser does not block the new tab.
  const tab = window.open('', '_blank');
  const { ok, answer } = await impPost('/api/admin/imports/spotify/connect', {}, null);
  if (!ok || !answer.url) {
    tab?.close();
    note(event.currentTarget, answer.message || 'Spotify could not be opened.', 'error');
    return;
  }
  if (tab) tab.location.href = answer.url; else location.href = answer.url;
  imp.waitingForPaste = !imp.view?.spotify?.octoFinishes;
  renderImports();
  if (imp.waitingForPaste) impEl('imp-paste').focus();
  else imp.timer = setTimeout(loadImports, 4000);
});

impEl('imp-finish')?.addEventListener('click', async event => {
  const address = impEl('imp-paste').value.trim();
  if (!address) { note(event.currentTarget, 'Paste the address first.', 'error'); return; }
  event.currentTarget.disabled = true;
  const { ok } = await impPost('/api/admin/imports/spotify/finish', { address }, event.currentTarget);
  event.currentTarget.disabled = false;
  if (ok) {
    imp.waitingForPaste = false;
    impEl('imp-paste').value = '';
    await loadImports();
  }
});

impEl('imp-paste')?.addEventListener('keydown', event => {
  if (event.key === 'Enter') { event.preventDefault(); impEl('imp-finish').click(); }
});

impEl('imp-read')?.addEventListener('click', async event => {
  await impPost('/api/admin/imports/spotify/read', {}, event.currentTarget);
  await loadImports();
});

impEl('imp-disconnect')?.addEventListener('click', async event => {
  if (!(await askConfirm('Disconnect Spotify?', 'Your lists, playlists and fetched songs stay. Octo stops reading your Spotify.', 'Disconnect'))) return;
  await impPost('/api/admin/imports/spotify/disconnect', {}, event.currentTarget);
  await loadImports();
});

impEl('imp-link-add')?.addEventListener('click', async event => {
  const url = impEl('imp-link').value.trim();
  if (!url) { note(event.currentTarget, 'Paste a link first.', 'error'); return; }
  event.currentTarget.disabled = true;
  note(event.currentTarget, 'Reading the link…', 'busy');
  const { ok, answer } = await impPost('/api/admin/imports/link', { url }, event.currentTarget);
  event.currentTarget.disabled = false;
  if (ok) {
    impEl('imp-link').value = '';
    await loadImports();
    if (answer.listId) await loadImportDetail(answer.listId);
  }
});

impEl('imp-file-add')?.addEventListener('click', async event => {
  const file = impEl('imp-file').files?.[0];
  if (!file) { note(event.currentTarget, 'Choose a file first.', 'error'); return; }
  const form = new FormData();
  form.append('file', file, file.name);
  event.currentTarget.disabled = true;
  note(event.currentTarget, 'Reading the file…', 'busy');
  const { ok, answer } = await impPost('/api/admin/imports/file', form, event.currentTarget);
  event.currentTarget.disabled = false;
  if (ok) {
    impEl('imp-file').value = '';
    await loadImports();
    if (answer.listId) await loadImportDetail(answer.listId);
  }
});

impEl('imp-lists')?.addEventListener('click', async event => {
  const open = event.target.closest('[data-imp-open]');
  if (open) { await loadImportDetail(open.dataset.impOpen); return; }
  const refresh = event.target.closest('[data-imp-refresh]');
  if (refresh) {
    refresh.disabled = true;
    await impPost(`/api/admin/imports/lists/${encodeURIComponent(refresh.dataset.impRefresh)}/refresh`, {}, null);
    await loadImports();
    return;
  }
  const remove = event.target.closest('[data-imp-remove]');
  if (remove) {
    const list = imp.view.lists.find(l => l.id === remove.dataset.impRemove);
    if (!(await askConfirm(`Remove ${list?.name ?? 'this list'}?`,
      'Octo forgets the list and stops getting its missing songs. A playlist it made stays in Navidrome, and fetched songs stay in your library.', 'Remove', true))) return;
    const response = await impFetch(`/api/admin/imports/lists/${encodeURIComponent(remove.dataset.impRemove)}`, { method: 'DELETE' });
    const answer = await response.json().catch(() => ({}));
    toast(answer.message || `HTTP ${response.status}`, response.ok ? 'ok' : 'error');
    if (imp.open === remove.dataset.impRemove) { imp.open = null; imp.detail = null; impEl('imp-detail').hidden = true; }
    await loadImports();
  }
});

impEl('imp-lists')?.addEventListener('change', async event => {
  const playlist = event.target.closest('[data-imp-playlist]');
  const fetchSwitch = event.target.closest('[data-imp-fetch]');
  const input = playlist ?? fetchSwitch;
  if (!input) return;
  const id = input.dataset.impPlaylist ?? input.dataset.impFetch;
  input.disabled = true;
  const { ok, answer } = await impPost(`/api/admin/imports/lists/${encodeURIComponent(id)}/${playlist ? 'playlist' : 'fetch'}`,
    { on: input.checked }, null);
  toast(answer.message || 'Done.', ok ? 'ok' : 'error');
  await loadImports();
});

impEl('imp-detail')?.addEventListener('change', event => {
  const pick = event.target.closest('[data-imp-pick]');
  if (!pick) return;
  if (pick.checked) imp.picked.add(pick.dataset.impPick); else imp.picked.delete(pick.dataset.impPick);
  renderImportDetail();
});

impEl('imp-detail')?.addEventListener('click', async event => {
  if (event.target.closest('#imp-detail-close')) {
    imp.open = null; imp.detail = null; impEl('imp-detail').hidden = true; renderImportLists();
    return;
  }
  if (event.target.closest('#imp-pick-all')) {
    for (const track of imp.detail.tracks) if (['missing', 'notFound', 'skipped'].includes(track.state)) imp.picked.add(track.key);
    renderImportDetail();
    return;
  }
  const get = event.target.closest('#imp-get');
  if (get && imp.picked.size) {
    const { ok, answer } = await impPost(`/api/admin/imports/lists/${encodeURIComponent(imp.open)}/songs`, { keys: [...imp.picked] }, null);
    toast(answer.message || 'Done.', ok ? 'ok' : 'error');
    imp.picked.clear();
    await loadImports();
  }
});

impEl('imp-pause')?.addEventListener('click', async event => {
  await impPost('/api/admin/imports/trickle/pause', { paused: imp.view?.trickle?.state !== 'paused' }, event.currentTarget);
  await loadImports();
});
impEl('imp-retry')?.addEventListener('click', async event => {
  await impPost('/api/admin/imports/trickle/retry', {}, event.currentTarget);
  await loadImports();
});
impEl('imp-clear')?.addEventListener('click', async event => {
  await impPost('/api/admin/imports/trickle/clear', {}, event.currentTarget);
  await loadImports();
});
impEl('imp-trickle-rows')?.addEventListener('click', async event => {
  const skip = event.target.closest('[data-imp-skip]');
  const retry = event.target.closest('[data-imp-retry]');
  if (!skip && !retry) return;
  const key = (skip ?? retry).dataset[skip ? 'impSkip' : 'impRetry'];
  await impPost(`/api/admin/imports/trickle/${skip ? 'skip' : 'retry'}`, { keys: [key] }, null);
  await loadImports();
});

// A saved Client ID or redirect URI changes what Connect can do, so the page reads itself again.
impEl('imports-form')?.addEventListener('submit', () => setTimeout(loadImports, 800));

// ────────────────────────────────────────────────────────────────
// Library health: the Octo app's checks and fixes, worked out on the server
// ────────────────────────────────────────────────────────────────
// GET /api/admin/health is the report; every fix is previewed first (POST .../preview), then
// run on the server one song at a time (POST .../apply, .../lookup) while this page follows
// GET .../run. A finished run says what it did beside an Undo. Only a Navidrome admin gets an
// answer; the fixes also need library actions on, with them on the allowed list.
// How many sets of copies or split albums, and how many songs, a finding shows before Show more.
const HEALTH_PAGE = 50;
const HEALTH_GROUPS = 12;
const healthPage = check => (check === 'duplicates' || check === 'splitAlbums' ? HEALTH_GROUPS : HEALTH_PAGE);
let healthReport = null;
let healthShown = {};
let healthTimer = null;
let healthHandledRun = null;
let healthLoading = false;

function healthPaneOpen() {
  return document.querySelector('section[data-pane="health"]')?.classList.contains('active') ?? false;
}

async function healthCall(path, body = undefined) {
  const options = body === undefined ? { cache: 'no-store' } : {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  };
  try {
    const r = await api(path, options);
    const data = await r.json().catch(() => ({}));
    return { ok: r.ok, status: r.status, data };
  } catch {
    return { ok: false, status: 0, data: { error: 'Octo did not answer.' } };
  }
}

const healthPlural = (n, one, many = `${one}s`) => `${(n ?? 0).toLocaleString()} ${n === 1 ? one : many}`;
const healthSong = id => healthReport?.songs?.[id] ?? { id, title: id };

async function loadHealth({ fresh = false } = {}) {
  const body = document.getElementById('health-body');
  if (!body || healthLoading) return;
  healthLoading = true;
  const refresh = document.getElementById('health-refresh');
  if (refresh) refresh.disabled = true;
  if (!healthReport || fresh) body.innerHTML = stateBlock('loading', fresh ? 'Reading your library from Navidrome again…' : 'Checking your library…');
  const { ok, data } = await healthCall(`/api/admin/health${fresh ? '?fresh=1' : ''}`);
  healthLoading = false;
  if (refresh) refresh.disabled = false;
  if (!ok) {
    healthReport = null;
    body.innerHTML = stateBlock('error', data.error || 'Library health could not be read.');
    return;
  }
  healthReport = data;
  renderHealth();
  followHealthRun(data.run, data.undoCount);
}

function renderHealth() {
  const report = healthReport;
  const body = document.getElementById('health-body');
  if (!report || !body) return;
  const read = report.readUtc ? ` Read from Navidrome ${relTime(report.readUtc)}.` : '';
  document.getElementById('health-overview').textContent = report.overview + read;

  const gate = document.getElementById('health-gate');
  gate.hidden = !report.can?.why;
  if (report.can?.why) {
    gate.innerHTML = `<span>${esc(report.can.why)} The findings are listed either way. </span>`
      + `<button type="button" class="link-button" data-open-tab="libraryactions">Open Library actions</button>`;
  }

  if (report.clean) {
    body.innerHTML = stateBlock('empty', report.allClear);
    return;
  }
  body.innerHTML = report.checks.map(healthSection).join('');
  // A preview or a lookup's review open before stays open.
  for (const [check, open] of Object.entries(healthPreviews)) {
    if (open.lookupRun) renderHealthLookups(open.lookupRun);
    else if (open.data) renderHealthPreview(check);
  }
}

// What a check offers: its fix-all button, and a lookup for songs a fill cannot reach.
function healthFixes(check) {
  const can = healthReport.can || {};
  const busy = healthReport.run?.state === 'running';
  const songs = check.songs || [];
  const batch = healthReport.lookupBatch || 25;
  const lookUp = can.lookUp && songs.length && ['noTrackNumber', 'noYear', 'noGenre', 'noAlbumArtist'].includes(check.check)
    ? `Look up ${songs.length > batch ? `the first ${batch}` : healthPlural(songs.length, 'song')}` : null;
  let all = null;
  if (check.check === 'duplicates' && can.remove) all = check.fixAllLabel;
  if (check.check === 'splitAlbums' && can.joinAlbums) all = check.fixAllLabel;
  if (check.check === 'noCover' && can.addCover) all = check.fixAllLabel;
  if (check.check === 'noLength' && can.upgrade) all = check.fixAllLabel;
  if (['noYear', 'noGenre', 'noAlbumArtist'].includes(check.check) && can.edit && check.fixAllLabel) all = check.fixAllLabel;
  return { all, lookUp, busy };
}

// Why a check's fix is not offered when the server could otherwise fix things from here.
function healthWhyNot(check, fixes) {
  const can = healthReport.can || {};
  if (can.why || fixes.all) return '';
  if (check.check === 'duplicates' && !can.remove) return 'Removing a copy needs the Delete action, which is off. Turn it on under Library actions to fix these here.';
  if (check.check === 'noLength' && !can.upgrade) return 'Finding a higher quality copy needs the Better quality action and Soulseek or Lidarr. Turn it on under Library actions.';
  if (check.check === 'noCover' && !can.addCover) return 'This server cannot look covers up.';
  if (['noYear', 'noGenre', 'noAlbumArtist'].includes(check.check) && can.edit && !check.fixAllLabel)
    return 'No song here has an album that agrees on it, so they can only be looked up.';
  return '';
}

function healthSection(check) {
  const fixes = healthFixes(check);
  const fixable = fixes.all || fixes.lookUp;
  const whyNot = healthWhyNot(check, fixes);
  const buttons = [
    fixes.lookUp ? `<button class="btn btn-ghost" type="button" data-health-lookup-all="${check.check}"${fixes.busy ? ' disabled' : ''}>${icon('i-magnifying-glass')}<span>${esc(fixes.lookUp)}</span></button>` : '',
    fixes.all ? `<button class="btn btn-primary" type="button" data-health-fix-all="${check.check}"${fixes.busy ? ' disabled' : ''}>${icon('i-check')}<span>${esc(fixes.all)}</span></button>` : '',
  ].join('');
  return `
    <div class="set-section health-check" data-check="${check.check}">
      <div class="set-head">
        <h3 class="set-title">${esc(check.title)}</h3>
        <p class="set-desc"><strong class="health-count">${esc(check.countLabel)}.</strong> ${esc(check.meaning)}</p>
      </div>
      <div class="set-card">
        <div class="set-row">
          <div class="set-info">
            <div class="set-info-t">${fixable ? 'Fix them' : 'What to do'}</div>
            <div class="set-info-d">${esc(fixable ? check.fixMeaning : check.advice)}</div>
            ${whyNot ? `<div class="set-info-d">${esc(whyNot)}${whyNot.includes('Library actions') ? ' <button type="button" class="link-button" data-open-tab="libraryactions">Open Library actions</button>' : ''}</div>` : ''}
            ${fixable ? '<div class="set-info-more">Every fix below shows what it changes first. The buttons on each item fix that one alone.</div>' : ''}
          </div>
          ${buttons ? `<div class="set-ctrl health-ctrl">${buttons}</div>` : ''}
        </div>
        <div class="set-row stack health-preview" id="health-preview-${check.check}" hidden></div>
      </div>
      ${healthItems(check)}
    </div>`;
}

function healthMore(check, total) {
  const shown = healthShown[check] ?? healthPage(check);
  if (total <= shown) return '';
  return `<div class="health-more"><button class="btn btn-ghost btn-sm" type="button" data-health-more="${check}">Show ${Math.min(healthPage(check), total - shown).toLocaleString()} more of ${(total - shown).toLocaleString()}</button></div>`;
}

function healthItems(check) {
  const shown = healthShown[check.check] ?? healthPage(check.check);
  const can = healthReport.can || {};
  const busy = healthReport.run?.state === 'running';
  const off = busy ? ' disabled' : '';
  if (check.check === 'duplicates') {
    const cards = check.sets.slice(0, shown).map(set => {
      const copies = set.copies.map(id => {
        const song = healthSong(id);
        const kept = id === set.keep;
        const side = kept
          ? '<span class="dl-badge good">Kept</span>'
          : can.remove ? `<button class="btn btn-ghost btn-sm" type="button" data-health-keep="${esc(set.key)}" data-id="${esc(id)}"${off}>Keep this one</button>` : '';
        return healthRow(song, `<div class="dl-sub">${esc(song.quality)}</div>`, side);
      }).join('');
      const fix = can.remove ? `<div class="health-actions"><button class="btn btn-sm" type="button" data-health-fix-set="${esc(set.key)}"${off}>Fix these copies</button></div>` : '';
      return `
        <div class="health-group">
          <div class="acq-head"><div class="dl-title">${esc(set.heading)}</div><span class="dl-badge state">${esc(set.basis)}</span></div>
          <div class="dl-sub">${esc(set.summary)}</div>
          <div class="health-copies">${copies}</div>
          <div class="dl-sub">${esc(set.line)}${set.note ? ` ${esc(set.note)}` : ''}</div>
          ${fix}
        </div>`;
    }).join('');
    return `<div class="health-groups">${cards}</div>${healthMore(check.check, check.sets.length)}`;
  }
  if (check.check === 'splitAlbums') {
    const cards = check.albums.slice(0, shown).map(album => {
      const parts = album.parts.map((part, index) => {
        const first = healthSong(part.songs[0]);
        const titles = part.songs.map(id => healthSong(id).title).join(', ');
        return `<div class="health-part">
          <div class="dl-by">${index === 0 ? 'Joined onto: ' : ''}${esc(healthPlural(part.songs.length, 'song'))}, album artist ${esc(first.albumArtist || 'none')}${first.year ? `, ${first.year}` : ''}</div>
          <div class="dl-sub">${esc(titles)}</div>
        </div>`;
      }).join('');
      const join = can.joinAlbums ? `<div class="health-actions"><button class="btn btn-sm" type="button" data-health-join="${esc(album.key)}"${off}>Join this album</button></div>` : '';
      return `
        <div class="health-group">
          <div class="acq-head"><div class="dl-title">${esc(album.heading)}</div></div>
          <div class="dl-sub">${esc(album.summary)}</div>
          <div class="health-copies">${parts}</div>
          <div class="dl-sub">${esc(album.join.words)}</div>
          ${join}
        </div>`;
    }).join('');
    return `<div class="health-groups">${cards}</div>${healthMore(check.check, check.albums.length)}`;
  }
  const fills = new Map((check.fills || []).map(fill => [fill.id, fill]));
  const rows = check.songs.slice(0, shown).map(id => {
    const song = healthSong(id);
    const fill = fills.get(id);
    const buttons = [
      fill && can.edit ? `<button class="btn btn-ghost btn-sm" type="button" data-health-fill="${check.check}" data-id="${esc(id)}"${off}>Fill in</button>` : '',
      can.lookUp && check.check !== 'noLength' && check.check !== 'noCover' ? `<button class="btn btn-ghost btn-sm" type="button" data-health-lookup="${check.check}" data-id="${esc(id)}"${off}>Look up</button>` : '',
      check.check === 'noCover' && can.addCover ? `<button class="btn btn-ghost btn-sm" type="button" data-health-cover="${esc(id)}"${off}>Find a cover</button>` : '',
      check.check === 'noLength' && can.upgrade ? `<button class="btn btn-ghost btn-sm" type="button" data-health-upgrade="${esc(id)}"${off}>Find higher quality</button>` : '',
    ].join('');
    const extra = fill ? `<div class="dl-sub">From its album: ${esc(fill.words)}</div>` : '';
    return healthRow(song, extra, buttons ? `<div class="health-actions">${buttons}</div>` : '');
  }).join('');
  return `<div class="dl-list health-songs">${rows}</div>${healthMore(check.check, check.songs.length)}`;
}

// A song row, as Fetched songs draws one: its title, who and where, then what can be done.
function healthRow(song, extra, side) {
  const by = [song.artist, song.album].filter(Boolean).map(esc).join(' <span class="dl-dash">·</span> ');
  const badge = song.quality ? `<span class="dl-badge ${song.lossless ? 'flac' : 'mp3'}">${esc(String(song.quality).split(',')[0])}</span>` : '';
  return `<div class="dl-item health-item">
    <div class="dl-main">
      <div class="dl-title">${esc(song.title)}</div>
      ${by ? `<div class="dl-by">${by}</div>` : ''}
      ${song.path ? `<div class="dl-path" title="${esc(song.path)}">${esc(song.path)}</div>` : ''}
      ${extra}
    </div>
    <div class="dl-side">${badge ? `<div class="dl-tags">${badge}</div>` : ''}${side}</div>
  </div>`;
}

// ── previews ─────────────────────────────────────────────────────
// Each preview opens in its finding's card and asks the server what the fix would do. Cancel
// closes it; the primary button runs exactly the steps shown.
let healthPreviews = {};

function healthPreviewHolder(check) {
  return document.getElementById(`health-preview-${check}`);
}

function closeHealthPreview(check) {
  const holder = healthPreviewHolder(check);
  if (holder) { holder.hidden = true; holder.innerHTML = ''; }
  delete healthPreviews[check];
}

async function openHealthPreview(check, request) {
  const holder = healthPreviewHolder(check);
  if (!holder) return;
  holder.hidden = false;
  holder.innerHTML = stateBlock('loading', 'Working out what this would change…');
  holder.scrollIntoView({ block: 'nearest', behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
  const { ok, data } = await healthCall('/api/admin/health/preview', { check, ...request });
  if (!ok) {
    holder.innerHTML = `${stateBlock('error', data.error || 'The preview could not be made.')}${healthPreviewButtons(check, null)}`;
    return;
  }
  healthPreviews[check] = { request, data };
  renderHealthPreview(check);
}

function healthPreviewButtons(check, go, enabled = true) {
  return `<div class="form-actions health-preview-actions">
    <button class="btn btn-ghost" type="button" data-health-cancel="${check}">Cancel</button>
    ${go ? `<button class="btn btn-primary" type="button" data-health-go="${check}"${enabled ? '' : ' disabled'}>${icon('i-check')}<span>${esc(go)}</span></button>` : ''}
  </div>`;
}

function healthLines(lines, limit = 120) {
  const shown = lines.slice(0, limit).join('');
  return shown + (lines.length > limit ? `<p class="set-info-d">And ${(lines.length - limit).toLocaleString()} more.</p>` : '');
}

function renderHealthPreview(check) {
  const holder = healthPreviewHolder(check);
  const preview = healthPreviews[check];
  if (!holder || !preview) return;
  const data = preview.data;
  let detail = '';
  if (data.sets) {
    const one = data.sets.length === 1;
    detail = healthLines(data.sets.map(set => `<div class="health-plan">
      <div class="set-info-t">${esc(set.heading)}</div>
      <div class="set-info-d">${esc(one ? set.why : set.line)}${set.note ? ` ${esc(set.note)}` : ''}</div>
      ${one ? healthDuplicateDetail(set, preview.request) : ''}
    </div>`));
  } else if (data.albums) {
    detail = healthLines(data.albums.map(album => `<div class="health-plan">
      <div class="set-info-t">${esc(album.heading)}</div>
      <div class="set-info-d">${esc([...album.reasons, album.words].join(' '))}</div>
      ${album.diff.map(part => `<div class="config-table health-diff" style="--cols: 2">
        <div class="config-row config-row-head"><span>${esc(healthPlural(part.songs, 'song'))} moving</span><span>Now</span><span>After</span></div>
        ${part.fields.map(field => `<div class="config-row${field.now !== field.after ? ' changes' : ''}"><span class="key">${esc(field.label)}</span><span class="value${field.now ? '' : ' empty'}">${esc(field.now ?? 'none')}</span><span class="value${field.after ? '' : ' empty'}">${esc(field.after ?? 'none')}</span></div>`).join('')}
      </div>`).join('')}
    </div>`));
  } else if (data.lines) {
    detail = `<div class="health-plan">${healthLines(data.lines.map(line => `<div class="set-info-d"><strong>${esc(line.title)}</strong>: ${esc(line.words)}</div>`))}</div>`;
  }
  const why = data.canFix === false && data.why ? `<div class="notice notice-warn">${esc(data.why)}</div>` : '';
  const steps = (data.steps || []).length;
  holder.innerHTML = `
    <div class="set-info">
      <div class="set-info-t">${esc(data.title)}</div>
      <div class="set-info-d">${esc(data.meaning || '')}</div>
    </div>
    ${why}
    <div class="health-plans">${detail}</div>
    ${healthPreviewButtons(check, data.canFix === false ? null : data.go, steps > 0 || !!data.upgrade)}`;
}

// One set of copies: every tag of every copy side by side, what the kept copy ends with, the
// blanks it can take from the others, and a pick where the copies disagree.
function healthDuplicateDetail(set, request) {
  const diff = set.diff;
  if (!diff) return '';
  const cols = diff.copies.length + 1;
  const head = diff.copies.map(copy => `<span>${copy.kept ? 'Kept, ' : ''}${esc(copy.quality)}</span>`).join('');
  const rows = diff.rows.map(row => `<div class="config-row${row.changes ? ' changes' : ''}">
    <span class="key">${esc(row.label)}</span>
    ${row.values.map(value => `<span class="value${value ? '' : ' empty'}">${esc(value ?? 'none')}</span>`).join('')}
    <span class="value${row.after ? '' : ' empty'}">${esc(row.after ?? 'none')}</span>
  </div>`).join('');
  const picked = request.fills ?? set.fills.map(fill => fill.tag);
  const fills = set.fills.length ? `<div class="health-picks">
    <div class="set-info-t">Fill in from the other copies</div>
    ${set.fills.map(fill => `<label class="lossy-check"><input type="checkbox" class="pick" data-health-fill-tag="${esc(fill.tag)}" data-key="${esc(set.key)}"${picked.includes(fill.tag) ? ' checked' : ''} /><span>${esc(fill.words)}</span></label>`).join('')}
  </div>` : '';
  const differs = set.differs.length ? `<div class="health-picks">
    <div class="set-info-t">Where the copies disagree</div>
    ${set.differs.map(choice => `<div class="health-choice" role="group" aria-label="${esc(choice.label)}">
      <span class="set-info-d">${esc(choice.label)}</span>
      ${[...new Set(choice.values.map(v => v.value))].map(value => `<button class="btn btn-ghost btn-sm" type="button" data-health-choose="${esc(choice.tag)}" data-key="${esc(set.key)}" data-value="${esc(value)}" aria-pressed="${choice.picked === value}">${esc(value)}</button>`).join('')}
    </div>`).join('')}
  </div>` : '';
  return `<div class="config-table health-diff" style="--cols: ${cols}">
      <div class="config-row config-row-head"><span>Tag</span>${head}<span>After</span></div>
      ${rows}
    </div>${fills}${differs}`;
}

async function runHealthPreview(check) {
  const preview = healthPreviews[check];
  if (!preview) return;
  const data = preview.data;
  const holder = healthPreviewHolder(check);
  const go = holder?.querySelector('[data-health-go]');
  if (data.upgrade) {
    // Better quality's own queue takes these, with its own gates and progress.
    if (go) go.disabled = true;
    const { ok, data: answer } = await healthCall('/api/admin/upgrades', { songs: data.upgrade });
    closeHealthPreview(check);
    showHealthResult(ok
      ? `Asked for a higher quality copy of ${healthPlural(answer.queued, 'song')}. Better quality shows how each one goes.`
      : answer.error || 'Better quality did not take them.', false);
    return;
  }
  await startHealthRun('/api/admin/health/apply', { label: data.label, check, steps: data.steps }, check);
}

async function startHealthRun(path, body, check) {
  const holder = healthPreviewHolder(check);
  const go = holder?.querySelector('[data-health-go]');
  if (go) go.disabled = true;
  const { ok, data } = await healthCall(path, body);
  if (!ok) {
    if (holder && !holder.hidden) note(holder.querySelector('.health-preview-actions') || holder, data.error || 'That did not start.', 'error');
    else showHealthResult(data.error || 'That did not start.', false);
    if (go) go.disabled = false;
    return;
  }
  if (path.endsWith('/apply') || path.endsWith('/undo')) closeHealthPreview(check);
  document.getElementById('health-result').hidden = true;
  if (healthReport) healthReport.run = data.run;
  renderHealth();
  followHealthRun(data.run, 0);
}

// ── runs ─────────────────────────────────────────────────────────
function renderHealthRun(run) {
  const wrap = document.getElementById('health-run-wrap');
  const list = document.getElementById('health-run');
  if (!wrap || !list) return;
  const running = run?.state === 'running';
  wrap.hidden = !running;
  if (!running) { list.innerHTML = ''; return; }
  const pct = run.total ? Math.round((run.done / run.total) * 100) : 0;
  const unit = run.kind === 'lookup' ? 'songs' : 'changes';
  list.innerHTML = `<div class="acq-card">
    <div class="acq-art">${icon(run.kind === 'lookup' ? 'i-magnifying-glass' : 'i-wrench')}</div>
    <div class="dl-main">
      <div class="acq-head">
        <div class="dl-title">${esc(run.label)}</div>
        <span class="dl-badge state">${run.done.toLocaleString()} of ${run.total.toLocaleString()} ${unit}</span>
      </div>
      ${run.current ? `<div class="dl-sub">Now: ${esc(run.current)}</div>` : ''}
      <div class="acq-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${pct}"><span style="width:${pct}%"></span></div>
      <div class="health-actions"><button class="btn btn-ghost btn-sm" type="button" id="health-stop">Stop after this song</button></div>
    </div>
  </div>`;
}

function followHealthRun(run, undoCount) {
  clearTimeout(healthTimer);
  renderHealthRun(run);
  if (!run) return;
  if (run.state === 'running') {
    if (healthPaneOpen()) healthTimer = setTimeout(pollHealthRun, 1200);
    return;
  }
  if (healthHandledRun === run.id) return;
  const fresh = !healthHandledRun;
  healthHandledRun = run.id;
  // A run that ended before this page was opened is not news; one that ended while it watched is.
  if (fresh && run.finishedUtc && Date.now() - Date.parse(run.finishedUtc) > 120000) return;
  if (run.kind === 'lookup') { renderHealthLookups(run); return; }
  const text = run.state === 'failed' ? `${run.label} stopped: ${run.error || 'something went wrong'}.` : run.summary;
  showHealthResult(text, run.kind === 'fix' && undoCount > 0);
}

async function pollHealthRun() {
  const { ok, data } = await healthCall('/api/admin/health/run');
  if (!ok) return;
  const was = healthReport?.run;
  if (healthReport) healthReport.run = data.run;
  if (data.run?.state === 'running') { followHealthRun(data.run, data.undoCount); return; }
  // Finished: the fixed songs leave their findings (the report is read again, which then says
  // how it went), and the buttons come back.
  if (was?.state === 'running' && data.run?.kind !== 'lookup') { loadHealth(); return; }
  renderHealth();
  followHealthRun(data.run, data.undoCount);
}

function showHealthResult(text, undo) {
  const box = document.getElementById('health-result');
  if (!box) return;
  document.getElementById('health-result-text').textContent = text;
  document.getElementById('health-undo').hidden = !undo;
  box.hidden = false;
}

// What a lookup found, each change picked when it fills a blank or the match is sure; nothing
// is written until the button below is pressed.
function renderHealthLookups(run) {
  const check = run.check || 'noTrackNumber';
  let holder = healthPreviewHolder(check);
  if (!holder) holder = healthPreviewHolder((healthReport?.checks || [])[0]?.check);
  if (!holder) return;
  const found = run.lookups.filter(lookup => lookup.state === 'found');
  const misses = run.lookups.filter(lookup => lookup.state !== 'found');
  const changes = found.reduce((sum, lookup) => sum + lookup.changes.length, 0);
  const blocks = found.filter(lookup => lookup.changes.length).map(lookup => `<div class="health-plan">
    <div class="set-info-t">${esc(lookup.title)}</div>
    <div class="set-info-d">${esc(lookup.origin || '')}</div>
    <div class="health-picks">${lookup.changes.map(change => `<label class="lossy-check"><input type="checkbox" class="pick" data-health-lookup-pick data-id="${esc(lookup.id)}" data-title="${esc(lookup.title)}" data-tag="${esc(change.tag)}" data-value="${esc(change.value)}"${change.pick ? ' checked' : ''} /><span>${esc(change.words)}</span></label>`).join('')}</div>
  </div>`).join('');
  const summary = !found.length
    ? (misses[0]?.detail || 'Nothing was found.')
    : !changes ? 'The tags already say what was found. Nothing to change.'
      : `Found tags for ${healthPlural(found.length, 'song')}. Sure matches and blanks are picked; look over the rest.`;
  holder.hidden = false;
  healthPreviews[holder.id.replace('health-preview-', '')] = { lookupRun: run };
  holder.innerHTML = `
    <div class="set-info">
      <div class="set-info-t">${esc(run.label)}</div>
      <div class="set-info-d">${esc(summary)}${misses.length && found.length ? ` ${esc(healthPlural(misses.length, 'song'))} found nothing.` : ''}</div>
    </div>
    <div class="health-plans">${blocks}</div>
    <div class="form-actions health-preview-actions">
      <button class="btn btn-ghost" type="button" data-health-cancel="${holder.id.replace('health-preview-', '')}">Close</button>
      ${changes ? `<button class="btn btn-primary" type="button" data-health-write="${esc(run.check || '')}">${icon('i-check')}<span>Write the picked changes</span></button>` : ''}
    </div>`;
}

function healthWriteLookups(check, holder) {
  const byId = new Map();
  holder.querySelectorAll('[data-health-lookup-pick]:checked').forEach(box => {
    const step = byId.get(box.dataset.id) || { action: 'retag', id: box.dataset.id, title: box.dataset.title, with: {} };
    step.with[box.dataset.tag] = box.dataset.value;
    byId.set(box.dataset.id, step);
  });
  const steps = [...byId.values()];
  if (!steps.length) { note(holder.querySelector('.health-preview-actions'), 'Nothing is picked.', 'info'); return; }
  startHealthRun('/api/admin/health/apply', { label: 'Writing tags', check: check || null, steps }, holder.id.replace('health-preview-', ''));
}

// ── wiring ───────────────────────────────────────────────────────
document.getElementById('health-refresh')?.addEventListener('click', () => loadHealth({ fresh: true }));
document.getElementById('health-result-close')?.addEventListener('click', () => { document.getElementById('health-result').hidden = true; });
document.getElementById('health-undo')?.addEventListener('click', async event => {
  event.currentTarget.disabled = true;
  const { ok, data } = await healthCall('/api/admin/health/undo', {});
  event.currentTarget.disabled = false;
  if (!ok) { showHealthResult(data.error || 'Undo did not start.', false); return; }
  document.getElementById('health-result').hidden = true;
  if (healthReport) healthReport.run = data.run;
  followHealthRun(data.run, 0);
});
document.getElementById('health-trash-link')?.addEventListener('click', () => {
  openTab('libraryactions');
  const trash = document.getElementById('trash-section');
  if (trash) setTimeout(() => trash.scrollIntoView({ block: 'start' }), 0);
});

document.querySelector('section[data-pane="health"]')?.addEventListener('click', event => {
  const target = event.target.closest('button, input');
  if (!target || !healthReport) return;
  const d = target.dataset;
  const checkOf = name => healthReport.checks.find(check => check.check === name);
  if (target.id === 'health-stop') { target.disabled = true; healthCall('/api/admin/health/stop', {}); return; }
  if (d.healthMore) { healthShown[d.healthMore] = (healthShown[d.healthMore] ?? healthPage(d.healthMore)) + healthPage(d.healthMore); renderHealth(); return; }
  if (d.healthCancel) { closeHealthPreview(d.healthCancel); return; }
  if (d.healthGo) { runHealthPreview(d.healthGo); return; }
  if (d.healthWrite !== undefined) { healthWriteLookups(d.healthWrite, target.closest('.health-preview')); return; }
  if (d.healthFixAll) { openHealthPreview(d.healthFixAll, {}); return; }
  if (d.healthLookupAll) {
    const check = checkOf(d.healthLookupAll);
    const ids = (check?.songs || []).slice(0, healthReport.lookupBatch || 25);
    startHealthRun('/api/admin/health/lookup', { ids, check: d.healthLookupAll }, d.healthLookupAll);
    return;
  }
  if (d.healthFixSet) { openHealthPreview('duplicates', { keys: [d.healthFixSet] }); return; }
  if (d.healthKeep) { openHealthPreview('duplicates', { keys: [d.healthKeep], keep: d.id }); return; }
  if (d.healthJoin) { openHealthPreview('splitAlbums', { keys: [d.healthJoin] }); return; }
  if (d.healthFill) { openHealthPreview(d.healthFill, { keys: [d.id] }); return; }
  if (d.healthCover) { openHealthPreview('noCover', { keys: [d.healthCover] }); return; }
  if (d.healthUpgrade) { openHealthPreview('noLength', { keys: [d.healthUpgrade] }); return; }
  if (d.healthLookup) { startHealthRun('/api/admin/health/lookup', { ids: [d.id], check: d.healthLookup }, d.healthLookup); return; }
  // A pick in one set's preview asks again, so the diff shows what the kept copy ends with.
  if (d.healthChoose || d.healthFillTag !== undefined) {
    const preview = healthPreviews.duplicates;
    if (!preview) return;
    const request = { ...preview.request, keys: [d.key] };
    if (d.healthChoose) request.choose = { ...(request.choose || {}), [d.healthChoose]: d.value };
    if (d.healthFillTag !== undefined) {
      request.fills = [...target.closest('.health-plan').querySelectorAll('[data-health-fill-tag]:checked')].map(box => box.dataset.healthFillTag);
    }
    openHealthPreview('duplicates', request);
  }
});

// ────────────────────────────────────────────────────────────────
// Boot
// ────────────────────────────────────────────────────────────────
ready.then(() => {
  if (location.hash) followHash();
  loadSettings();
  loadAmbient();
  activityTick();
  loadSignedIn();
  loadUpdate();
  renderSlskdOpen();
});
