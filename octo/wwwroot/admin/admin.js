// Octo admin UI. Vanilla JS, no build step.

// Every call to Octo's admin API goes through here. Writes carry X-Octo-Admin, which a page on
// another origin cannot add without a preflight Octo refuses, so a site you happen to visit
// cannot change settings or restart Octo on your behalf. It is not a login: see README.
function api(url, options = {}) {
  const method = (options.method || 'GET').toUpperCase();
  const headers = new Headers(options.headers || {});
  if (method !== 'GET' && method !== 'HEAD') headers.set('X-Octo-Admin', '1');
  return fetch(url, { credentials: 'same-origin', ...options, headers });
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
  if (location.hash !== `#${name}`) history.replaceState(null, '', `#${name}`);
  // Segmented thumbs can only be measured once their pane is visible.
  if (typeof syncSegments === 'function') syncSegments(false);
  // Panes that show live data reload whenever they are opened, so they are never stale.
  if (name === 'fetched' && typeof loadFetched === 'function') loadFetched();
  if (name === 'raw' && typeof loadRawConfig === 'function') loadRawConfig();
  if (name === 'sources' && typeof loadConfigSources === 'function') loadConfigSources();
  if (name === 'lastfm' && typeof loadRadioStatus === 'function') loadRadioStatus();
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
refreshStatus();
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
  loadRadioStatus();

  // Meta references
  const cfgPath = document.getElementById('meta-config-path');
  if (cfgPath && currentSettings?._meta?.ConfigFilePath) {
    cfgPath.textContent = currentSettings._meta.ConfigFilePath;
  }
  const version = document.getElementById('meta-version');
  if (version && currentSettings?._meta?.Version) {
    version.textContent = currentSettings._meta.Version;
  }
  const slskdLink = document.getElementById('slskd-link');
  if (slskdLink) {
    const here = new URL(location.href);
    slskdLink.href = `${here.protocol}//${here.hostname}:5030`;
    slskdLink.textContent = `${here.hostname}:5030`;
  }

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
  const note = actions?.querySelector('.unsaved-status');
  const noteText = unsaved ? 'Unsaved changes' : '';
  // Only when it changes: rewriting it swaps the text node, which the form's MutationObserver
  // would take as an edit and check again, every frame, and a live region would re-announce.
  if (note && note.textContent !== noteText) note.textContent = noteText;
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
    <button type="submit" class="btn btn-primary" ${currentSettings ? '' : 'disabled'}>Save</button>
    ${form.id === 'lidarr-connection-form' ? '<button type="button" class="btn btn-ghost" id="lidarr-test-connection">Test connection</button>' : ''}
    <span class="unsaved-status" aria-live="polite"></span>
    <span class="saved-status"></span>
    <span class="restart-hint">
      <svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
        <circle cx="8" cy="8" r="6"/><path d="M8 5v3M8 10.5v.5"/>
      </svg>
      Restart required for one or more changes
    </span>
  `;
  form.appendChild(actions);
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
          && !confirm('Turn off rehearsal mode? From the next check, a track added to an action playlist, or rated if ratings are on, moves a real file into quarantine.')) return;
    }

    const { patch, needsRestart, invalid } = collectPatch(form);
    if (invalid) {
      const holder = invalid.dataset.errorFor ? document.getElementById(invalid.dataset.errorFor) : null;
      const message = "This isn't valid JSON, so nothing was saved.";
      if (holder) {
        holder.textContent = message;
        holder.hidden = false;
      }
      toast(message, 'error');
      return;
    }
    form.querySelectorAll('.field-error[data-json-error]').forEach(el => { el.hidden = true; });

    const status = form.querySelector('.saved-status');
    const submit = form.querySelector('button[type="submit"]');
    submit.disabled = true;
    status.textContent = 'Saving…';

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
        toast('Saved. Reload the page to see the current values.', 'ok');
      }
      if (form.id === 'lidarr-connection-form') await loadLidarrOptions();
      renderOctoAddresses();
      renderRestartPending();
      renderSetupChecklist();
      status.textContent = `Saved · ${new Date().toLocaleTimeString()}`;
      form.querySelector('.form-actions')?.classList.remove('dirty');
      // A saved admin password is only ever shown as the placeholder; the typed value should
      // not stay in the page after it has been saved.
      form.querySelectorAll('input[type="password"][name]').forEach(input => {
        const [section, key] = input.name.split('.');
        const saved = currentSettings?.[section]?.[key];
        if (saved && saved === currentSettings?._meta?.SecretPlaceholder) input.value = saved;
      });
      markClean(form);
      toast(needsRestart
        ? 'Saved. Restart for these to take effect.'
        : 'Settings saved.', 'ok');
    } catch (err) {
      status.textContent = '';
      toast(`Save failed: ${err.message}`, 'error');
    } finally {
      submit.disabled = false;
    }
  });
});

// Heart acquisition is a short priority chain, not a workflow graph. Keep all
// sources visible so disabling one never destroys the user's chosen order.
const heartSourceMeta = {
  Soulseek: { title: 'Soulseek', detail: 'Lossless FLAC from slskd peers' },
  YouTube: { title: 'YouTube', detail: 'Lossy MP3 from the yt-dlp shim' },
  Lidarr: { title: 'Lidarr', detail: 'Album automation through your Lidarr server' },
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
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3h1M10 3h1M5 8h1M10 8h1M5 13h1M10 13h1"/></svg>
        </button>
        <span class="source-step" aria-hidden="true">${index + 1}</span>
        <span class="source-copy">
          <span class="source-title">${meta.title}</span>
          <span class="source-detail">${meta.detail}</span>
        </span>
        <span class="source-heart-controls" role="group" aria-label="${meta.title} heart types">
          <span class="source-heart-choice">
            <span class="source-heart-label">Song hearts
              ${step.Source === 'Lidarr' ? `
                <span class="source-info" tabindex="0" role="img"
                      aria-label="A song heart asks Lidarr to acquire the entire album. Enable this if you want Lidarr to handle single-song requests anyway."
                      data-tooltip="A song heart asks Lidarr to acquire the entire album. Enable this if you want Lidarr to handle single-song requests anyway.">
                  <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6"/><path d="M8 7v4M8 4.8v.1"/></svg>
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

document.getElementById('rejected-peers-clear')?.addEventListener('click', async () => {
  const count = currentSettings?._meta?.RejectedPeerCount ?? 0;
  if (!count) return;
  if (!confirm(`Forget ${count} rejected peer${count === 1 ? '' : 's'}? Those files become downloadable again.`)) return;
  try {
    const response = await api('/api/admin/soulseek/rejected-peers/clear', { method: 'POST' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();
    toast(`Forgot ${body.cleared} rejected peer${body.cleared === 1 ? '' : 's'}.`);
    // Only the count changed. Reloading every form here threw away unsaved edits elsewhere.
    currentSettings = await (await api('/api/admin/settings', { cache: 'no-store' })).json();
    renderRejectedPeerCount();
  } catch (error) {
    toast(`Could not clear: ${error.message}`, 'err');
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
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3h1M10 3h1M5 8h1M10 8h1M5 13h1M10 13h1"/></svg>
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
        <button class="btn btn-ghost" type="button" data-genre-remove aria-label="Remove rule ${index + 1}">Remove</button>
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
  if (existing > 0 && !confirm(`Replace your ${existing} rule${existing === 1 ? '' : 's'} with the broad preset? Nothing is saved until you press Save.`)) return;
  try {
    const response = await api('/api/admin/genre/presets');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();
    renderGenreMappings(body.broad);
    toast(`Loaded ${body.broad.length} rules. Save to apply.`);
  } catch (error) {
    toast(`Could not load the preset: ${error.message}`, 'err');
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
      <div class="set-info-d">${esc(counts)}${run.reason ? ` — ${esc(run.reason)}` : ''}</div>
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
  if (!response.ok) return null;
  const run = await response.json();
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
    toast(`${run.dryRun ? 'Preview' : 'Genre run'} ${run.status === 'Completed' ? 'finished' : run.status.toLowerCase()}: ${run.changed} file(s)${run.dryRun ? ' would change' : ' changed'}.`,
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

async function startGenreBackfill(dryRun, scopeOverride = null) {
  const scope = scopeOverride ?? document.getElementById('genre-backfill-scope')?.value ?? 'OctoDownloads';

  let confirmPath = null;
  if (scope === 'WholeLibrary' && !dryRun) {
    const current = await loadGenreBackfill();
    const expected = current?.musicPath ?? '';
    confirmPath = prompt(
      `This rewrites tags on every audio file under:\n\n${expected}\n\n` +
      'including music Octo never downloaded. Type that path exactly to continue.');
    if (confirmPath === null) return;
  }

  const response = await genreBackfillFetch('/api/admin/genre/backfill', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ scope, dryRun, confirm: confirmPath }),
  });

  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    toast(body.error || `Could not start: HTTP ${response.status}`, 'error');
    return;
  }
  toast(dryRun ? 'Previewing. Nothing is being written.' : 'Applying changes.');
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
  if (!confirm(`Write new genres to ${run.changed} file(s) in ${scopeLabel}? Only the genre is recorded for undo; anything else the tag library cannot round-trip is lost.`)) return;
  // The previewed scope, not whatever the dropdown says by now.
  await startGenreBackfill(false, run.scope);
});
document.getElementById('genre-backfill-cancel')?.addEventListener('click', async () => {
  const response = await genreBackfillFetch('/api/admin/genre/backfill/cancel', { method: 'POST' });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    toast(body.error || `Could not cancel: HTTP ${response.status}`, 'error');
    return;
  }
  toast('Cancelling after the current file.');
  await loadGenreBackfill();
});
document.getElementById('genre-backfill-resume')?.addEventListener('click', async () => {
  const run = lastBackfillRun;
  // Resuming a preview writes nothing; resuming an apply writes tags, so it asks again.
  if (run && !run.dryRun
      && !confirm(`Resume writing genres from file ${run.processed + 1} of ${run.total}?`)) return;
  const response = await genreBackfillFetch('/api/admin/genre/backfill/resume', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { toast(body.error || 'Could not resume.', 'error'); return; }
  await loadGenreBackfill();
});
document.getElementById('genre-backfill-undo')?.addEventListener('click', async () => {
  if (!confirm('Put back the genre on every file changed since the last undo? Only the genre is restored, and files moved since then stay changed.')) return;
  const response = await genreBackfillFetch('/api/admin/genre/backfill/undo', { method: 'POST' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { toast(body.error || 'Could not undo.', 'error'); return; }
  toast('Restoring genres.');
  await loadGenreBackfill();
});

// ---- Library actions -----------------------------------------------------

let libraryActions = [];

const libraryActionLabels = {
  Delete: 'Delete the file and never ask for it again',
  WrongSong: 'Wrong song: replace it, and blacklist the peer that sent it',
  WrongVersion: 'Wrong version: find the plain recording instead',
  BetterQuality: 'Better quality: upgrade only to a larger lossless copy',
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

document.getElementById('library-actions-refresh')?.addEventListener('click', async () => {
  const holder = document.getElementById('library-actions-history');
  if (!holder) return;
  try {
    let response = await api('/api/admin/library-actions', { credentials: 'same-origin' });
    if (response.status === 401 && await browseAuthenticate(holder)) {
      response = await api('/api/admin/library-actions', { credentials: 'same-origin' });
    }
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body = await response.json();

    if (!body.entries?.length) { holder.innerHTML = '<p class="set-info-d">Nothing yet.</p>'; return; }
    holder.innerHTML = `
      <div class="config-table">
        <div class="config-row config-row-head genre-change-row">
          <span>Track</span><span>Action</span><span>Who</span><span>Result</span>
        </div>
        ${body.entries.map(entry => `
          <div class="config-row genre-change-row">
            <span class="key">${esc(entry.artist)} - ${esc(entry.title)}</span>
            <span class="value">${esc(entry.action)}${entry.dryRun ? ' (rehearsal)' : ''}</span>
            <span class="value">${esc(entry.username)}</span>
            <span class="value">${esc(entry.state)}${entry.detail ? `: ${esc(entry.detail)}` : ''}</span>
          </div>`).join('')}
      </div>`;
  } catch (error) {
    holder.innerHTML = `<div class="field-error" role="alert">${esc(error.message)}</div>`;
  }
});

document.getElementById('lidarr-test-connection')?.addEventListener('click', async (event) => {
  const button = event.currentTarget;
  const form = document.getElementById('lidarr-connection-form');
  const status = form?.querySelector('.saved-status');
  const baseUrl = document.getElementById('f-lidarr-url')?.value ?? '';
  const apiKey = document.getElementById('f-lidarr-key')?.value ?? '';
  button.disabled = true;
  if (status) status.textContent = 'Testing…';
  try {
    const r = await api('/api/admin/lidarr/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ baseUrl, apiKey }),
    });
    const result = await r.json();
    if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
    populateLidarrOptions(result.options || {});
    if (status) status.textContent = result.message || 'Connected to Lidarr.';
    toast(result.message || 'Connected to Lidarr.', 'ok');
  } catch (err) {
    if (status) status.textContent = `Test failed: ${err.message}`;
    toast(`Lidarr test failed: ${err.message}`, 'error');
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
  // Live JSON validation as the user types — surface errors before save.
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
      toast('Fix JSON errors before saving.', 'error');
      return;
    }
    if (!confirm('Replace settings.json with exactly this text? Every value shown here, including ones that came from environment variables, is written into the file and overrides .env from now on. Every settings card reloads afterwards.')) return;

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
      if (rawSavedStatus) rawSavedStatus.textContent = `Saved · ${new Date().toLocaleTimeString()} · ${result.bytes} bytes`;
      rawDirty = false;
      toast('Settings file saved.', 'ok');
      // Refresh the form-by-form view so any open tab reflects changes.
      await loadSettings();
    } catch (err) {
      if (rawSavedStatus) rawSavedStatus.textContent = '';
      toast(`Save failed: ${err.message}`, 'error');
    } finally {
      submit.disabled = false;
    }
  });

  document.getElementById('raw-reload')?.addEventListener('click', async () => {
    if (rawDirty && !confirm('Discard your edits and reload settings.json from disk?')) return;
    await loadRawConfig(true);
    if (rawSavedStatus) rawSavedStatus.textContent = `Reloaded · ${new Date().toLocaleTimeString()}`;
    toast('Reloaded from disk.', 'ok');
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
    configTable.querySelector('.config-loading').textContent = `failed: ${e.message}`;
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
  if (!confirm(`Restart the Octo container? In-flight requests drop. Service comes back in 5-10s.${unsavedNote}`)) return;
  const btn = document.getElementById('restart-btn');
  const label = btn.querySelector('span');
  btn.disabled = true;
  if (label) label.textContent = 'Restarting…';
  toast('Restart triggered. Waiting for service…');

  try { await api('/api/admin/restart', { method: 'POST' }); }
  catch { /* expected — connection drops */ }

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
document.getElementById('radio-discovery-list')?.addEventListener('click', event => {
  const button = event.target.closest('[data-radio-action]'); const row = button?.closest('.radio-discovery-row');
  if (!button || !row) return; readRadioDiscoveryRows(); const index = Number(row.dataset.index);
  if (button.dataset.radioAction === 'remove') { if (!confirm(`Remove “${radioDiscoveryStations[index].Name}”? Listening history and downloaded music are untouched.`)) return; radioDiscoveryStations.splice(index, 1); }
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
  const user = document.getElementById('radio-user')?.value; if (!user || !confirm(`Reset Radio history for “${user}”? Downloaded music will not be removed.`)) return;
  const button = event.currentTarget;
  button.disabled = true;
  try { const response = await api(`/api/admin/lastfm/radio/history?user=${encodeURIComponent(user)}`, { method: 'DELETE' }); const data = await response.json();
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`); toast(data.message || 'Radio history reset.'); await loadRadioStatus();
  } catch (error) { toast(`Reset failed: ${error.message}`, 'error'); } finally { button.disabled = false; }
});

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
    box.textContent = !d.configured ? d.detail : d.valid ? `Valid · ${d.userName}` : `Not valid — ${d.detail}`;
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
      `${s.sink}: ${!s.configured ? 'not configured' : s.ok ? 'OK' : 'failed — ' + s.detail}`);
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
      bits.push('Auto-detect is off, so the path below is used verbatim.');
    }
    bits.push(`Downloads go to <code>${esc(s.effectiveDownloadPath || '(unset)')}</code>${s.writable ? '' : ' — <strong>not writable by Octo</strong>'}.`);
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
refreshLibraryStatus();

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
    // Clicking the backdrop dismisses, but only the backdrop itself — a drag
    // that starts inside the card must not count as an outside click.
    const onBackdrop = (e) => { if (e.target === modal) onCancel(); };

    submit.addEventListener('click', onSubmit);
    cancel.addEventListener('click', onCancel);
    modal.addEventListener('keydown', onKey);
    modal.addEventListener('mousedown', onBackdrop);
  });
}

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
  return true;   // the session is now in the cookie the response set
}

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
    ? '<div class="detect-tag">Octo runs in a container, so this is what it can see — the host\'s own drives are not visible unless mounted.</div>'
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
  const original = detectBtn.textContent;
  detectBtn.textContent = 'Scanning…';
  if (result) { result.hidden = false; result.textContent = 'Scanning the local network…'; }
  try {
    const r = await api('/api/admin/discover-servers', { cache: 'no-store' });
    const data = await r.json();
    const servers = data.servers || [];
    if (!servers.length) {
      if (result) result.innerHTML =
        'No server found on this network. If Octo runs in a Docker bridge network it cannot see your LAN — use host networking, or enter the URL manually.';
    } else if (servers.length === 1) {
      urlInput.value = servers[0].url;
      urlInput.dispatchEvent(new Event('input', { bubbles: true }));
      if (result) result.innerHTML =
        `Found <strong>${esc(servers[0].type || 'Subsonic')}</strong> ${esc(servers[0].serverVersion || '')} at <code>${esc(servers[0].url)}</code>, filled in above. Save to apply.`;
    } else {
      const rows = servers.map(s =>
        `<button type="button" class="btn btn-ghost detect-pick" data-url="${esc(s.url)}">${esc(s.url)} <span class="detect-tag">${esc(s.type || 'subsonic')} ${esc(s.serverVersion || '')}</span></button>`).join('');
      if (result) result.innerHTML = `Found ${servers.length} servers — pick one:<div class="detect-list">${rows}</div>`;
      result.querySelectorAll('.detect-pick').forEach(b =>
        b.addEventListener('click', () => {
          urlInput.value = b.dataset.url;
          urlInput.dispatchEvent(new Event('input', { bubbles: true }));
          if (typeof toast === 'function') toast('URL filled in — Save to apply.', 'ok');
        }));
    }
  } catch (e) {
    if (result) result.textContent = 'Scan failed: ' + (e?.message || 'unknown error');
  } finally {
    detectBtn.disabled = false;
    detectBtn.textContent = original;
  }
});

// ────────────────────────────────────────────────────────────────
// Fetched songs — running download log
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
async function loadFetched() {
  const list = document.getElementById('fetched-list');
  if (!list) return;
  try {
    const r = await api('/api/admin/downloads', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();
    const items = data.downloads || [];
    if (!items.length) {
      list.innerHTML = '<div class="dl-empty">Nothing fetched yet. Heart a song Octo found for you in your music app, and it shows up here once it is in your library.</div>';
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
      return `<div class="dl-item">
        ${art}
        <div class="dl-main">
          <div class="dl-title">${escapeHtml(d.artist)} <span class="dl-dash">—</span> ${escapeHtml(d.title)}</div>
          <div class="dl-path" title="${escapeHtml(d.path)}">${escapeHtml(d.path)}</div>
        </div>
        <div class="dl-side">
          <div class="dl-tags"><span class="dl-badge ${badgeClass}">${escapeHtml(fmt)}</span><span class="dl-source">${escapeHtml(d.source)}</span></div>
          <div class="dl-sub">${escapeHtml(relTime(d.downloadedAt))}${size ? ' · ' + size : ''}${who ? ' · ' + who : ''}</div>
        </div>
      </div>`;
    }).join('');
  } catch (e) {
    list.innerHTML = `<div class="dl-empty">Couldn't load the log: ${escapeHtml(e.message || 'error')}</div>`;
  }
}
document.getElementById('fetched-refresh')?.addEventListener('click', loadFetched);

// ────────────────────────────────────────────────────────────────
// Segmented controls — buttons built from a hidden <select> they proxy to,
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
      if (typeof toast === 'function') toast('Address copied.', 'ok');
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
    row.querySelectorAll('input:not([type="hidden"]), select, textarea').forEach(ctrl => {
      if (ctrl.closest('.source-priority-row, .radio-discovery-row')) return;
      // A switch's <label> wraps only the track and thumb, so it gives no name of its own.
      const namedByLabel = Array.from(ctrl.labels || []).some(label => label.textContent.trim());
      if (namedByLabel || ctrl.hasAttribute('aria-label') || ctrl.hasAttribute('aria-labelledby')) return;
      ctrl.setAttribute('aria-labelledby', title.id);
      if (desc) ctrl.setAttribute('aria-describedby', desc.id);
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
    if (source === 'Soulseek') return services.slskd?.ok;
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
      <button type="button" class="btn btn-ghost" data-open-tab="${row.tab}" aria-label="Open ${esc(row.title)}">Open</button>
    </li>`).join('')}</ol>`;
  // Only when something changed, so a status refresh does not pull focus off a button.
  if (holder.dataset.rendered !== markup) {
    holder.innerHTML = markup;
    holder.dataset.rendered = markup;
  }

  const required = rows.filter(row => row.required);
  const missing = required.filter(row => row.state === 'bad');
  const checking = required.some(row => row.state === 'pending');
  summary.textContent = checking ? 'Checking setup…'
    : missing.length ? `Setup: ${missing.length} of ${required.length} required step${required.length === 1 ? '' : 's'} need${missing.length === 1 ? 's' : ''} attention`
      : 'Setup complete. Point your apps at the address below.';
  wrap.classList.toggle('needs-attention', missing.length > 0);
  // Open when something required is missing; otherwise stay out of the way, unless the user
  // opened it themselves.
  if (!wrap.dataset.userToggled) wrap.open = missing.length > 0;
}
// Once the user opens or closes it themselves, stop opening and closing it for them.
document.getElementById('setup-summary')?.addEventListener('click', () => {
  document.getElementById('setup-checklist-wrap').dataset.userToggled = '1';
});

// ────────────────────────────────────────────────────────────────
// Boot
// ────────────────────────────────────────────────────────────────
if (location.hash) followHash();
loadSettings();
