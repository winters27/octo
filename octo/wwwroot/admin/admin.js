// Octo admin UI. Vanilla JS, no build step.

// ────────────────────────────────────────────────────────────────
// Sidebar nav: tab switching
// ────────────────────────────────────────────────────────────────
const navItems = document.querySelectorAll('.sidebar-nav-item');
const panes    = document.querySelectorAll('section[data-pane]');

function activateTab(name) {
  navItems.forEach(b => b.classList.toggle('active', b.dataset.tab === name));
  panes.forEach(p => p.classList.toggle('active', p.dataset.pane === name));
  if (location.hash !== `#${name}`) location.hash = name;
}

navItems.forEach(btn => btn.addEventListener('click', () => activateTab(btn.dataset.tab)));

// Honor #hash on load
if (location.hash) {
  const target = location.hash.slice(1);
  if (document.querySelector(`section[data-pane="${target}"]`)) activateTab(target);
}

// ────────────────────────────────────────────────────────────────
// Toast helper
// ────────────────────────────────────────────────────────────────
const toastEl = document.getElementById('toast');
let toastTimer = null;
function toast(msg, kind = 'ok') {
  toastEl.textContent = msg;
  toastEl.className = `show ${kind}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { toastEl.className = ''; }, 3500);
}

// ────────────────────────────────────────────────────────────────
// Status grid + sidebar badge
// ────────────────────────────────────────────────────────────────
const statusBadge      = document.querySelector('[data-status-badge]');
const statusLastChecked = document.getElementById('status-last-checked');

async function refreshStatus() {
  try {
    const r = await fetch('/api/admin/status');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();

    setStatusCard('octo', data.octo);
    Object.entries(data.services || {}).forEach(([k, v]) => setStatusCard(k, v));

    // Update sidebar badge with bad count, if any
    const all = [data.octo, ...Object.values(data.services || {})];
    const badCount = all.filter(s => !s?.ok).length;
    if (statusBadge) {
      if (badCount > 0) {
        statusBadge.className = 'sidebar-nav-badge bad';
        statusBadge.textContent = String(badCount);
      } else {
        statusBadge.className = 'sidebar-nav-badge';
        statusBadge.textContent = '';
      }
    }
    if (statusLastChecked) {
      statusLastChecked.textContent = new Date().toLocaleTimeString();
    }
  } catch (e) {
    document.querySelectorAll('.status-card').forEach(card => {
      card.classList.add('bad');
      const dot = card.querySelector('.status-dot');
      const body = card.querySelector('.status-card-body');
      if (dot) dot.className = 'status-dot bad';
      if (body) body.textContent = `probe failed: ${e.message}`;
    });
  }
}

function setStatusCard(svc, probe) {
  const card = document.querySelector(`.status-card[data-svc="${svc}"]`);
  if (!card) return;
  card.classList.toggle('bad', !probe.ok);
  const dot = card.querySelector('.status-dot');
  const body = card.querySelector('.status-card-body');
  if (dot)  dot.className  = `status-dot ${probe.ok ? 'ok' : 'bad'}`;
  if (body) body.textContent = probe.detail || (probe.ok ? 'online' : 'unreachable');
}

document.getElementById('status-refresh')?.addEventListener('click', refreshStatus);
refreshStatus();
setInterval(refreshStatus, 30_000);

// ────────────────────────────────────────────────────────────────
// Settings: load -> populate forms -> save on submit
// ────────────────────────────────────────────────────────────────
let currentSettings = null;

async function loadSettings() {
  const r = await fetch('/api/admin/settings');
  if (!r.ok) {
    toast(`Failed to load settings: HTTP ${r.status}`, 'error');
    return;
  }
  currentSettings = await r.json();

  // Populate every <input>/<select> whose name="Section.Key" matches.
  document.querySelectorAll('[name]').forEach(el => {
    if (!el.name?.includes('.')) return;
    const [section, key] = el.name.split('.');
    const value = currentSettings?.[section]?.[key];
    if (value === undefined || value === null) return;
    if (el.type === 'checkbox') el.checked = !!value;
    else el.value = value;
  });

  // Meta references
  const cfgPath = document.getElementById('meta-config-path');
  if (cfgPath && currentSettings?._meta?.ConfigFilePath) {
    cfgPath.textContent = currentSettings._meta.ConfigFilePath;
  }
  const slskdLink = document.getElementById('slskd-link');
  if (slskdLink) {
    const here = new URL(location.href);
    slskdLink.href = `${here.protocol}//${here.hostname}:5030`;
    slskdLink.textContent = `${here.hostname}:5030`;
  }

  // Initial dirty-check pass: all forms start clean.
  document.querySelectorAll('form[data-section]').forEach(form => {
    form.querySelector('.form-actions')?.classList.remove('dirty');
  });
}

// ────────────────────────────────────────────────────────────────
// Inject Save bar into every settings card
// ────────────────────────────────────────────────────────────────
function ensureSaveBar(form) {
  if (form.querySelector('.form-actions')) return;
  const actions = document.createElement('div');
  actions.className = 'form-actions';
  actions.innerHTML = `
    <button type="submit" class="btn btn-primary">Save</button>
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

document.querySelectorAll('form[data-section]').forEach(form => {
  ensureSaveBar(form);

  // Mark form dirty whenever a restart-required input changes.
  form.addEventListener('input', () => {
    const dirty = Array.from(form.querySelectorAll('[data-restart="true"]'))
      .some(el => isFieldDirty(el));
    form.querySelector('.form-actions')?.classList.toggle('dirty', dirty);
  });

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const patch = {};
    let needsRestart = false;

    form.querySelectorAll('[name]').forEach(el => {
      if (!el.name?.includes('.')) return;
      const [section, key] = el.name.split('.');
      patch[section] = patch[section] || {};
      let value;
      if (el.type === 'checkbox') value = el.checked;
      else if (el.type === 'number') {
        value = el.value === '' ? null : Number(el.value);
        if (Number.isNaN(value)) value = null;
      } else value = el.value;
      patch[section][key] = value;

      if (el.dataset.restart === 'true' && isFieldDirty(el)) {
        needsRestart = true;
      }
    });

    const status = form.querySelector('.saved-status');
    const submit = form.querySelector('button[type="submit"]');
    submit.disabled = true;
    status.textContent = 'Saving…';

    try {
      const r = await fetch('/api/admin/settings', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      });
      const result = await r.json();
      if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);

      currentSettings = await (await fetch('/api/admin/settings')).json();
      status.textContent = `Saved · ${new Date().toLocaleTimeString()}`;
      form.querySelector('.form-actions')?.classList.remove('dirty');
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

// ────────────────────────────────────────────────────────────────
// Raw config editor
// ────────────────────────────────────────────────────────────────
const rawEditor = document.getElementById('raw-editor');
const rawError  = document.getElementById('raw-error');
const rawForm   = document.getElementById('raw-form');

async function loadRawConfig() {
  if (!rawEditor) return;
  try {
    const r = await fetch('/api/admin/raw-config');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    rawEditor.value = await r.text();
    rawError.hidden = true;
  } catch (e) {
    rawEditor.value = '// failed to load: ' + e.message;
  }
}

if (rawForm) {
  // Live JSON validation as the user types — surface errors before save.
  rawEditor.addEventListener('input', () => {
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

    const submit = rawForm.querySelector('button[type="submit"]');
    submit.disabled = true;
    if (rawSavedStatus) rawSavedStatus.textContent = 'Saving…';

    try {
      const r = await fetch('/api/admin/raw-config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: val,
      });
      const result = await r.json();
      if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
      if (rawSavedStatus) rawSavedStatus.textContent = `Saved · ${new Date().toLocaleTimeString()} · ${result.bytes} bytes`;
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
    await loadRawConfig();
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
    const r = await fetch('/api/admin/config-sources');
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
        <span class="key">${row.Key}</span>
        <span class="${valueClass}">${escapeHtml(valueText)}</span>
      `;
      configTable.appendChild(div);
    }
  } catch (e) {
    configTable.querySelector('.config-loading').textContent = `failed: ${e.message}`;
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
  })[c]);
}

// Re-load these whenever their tab activates so they're never stale.
navItems.forEach(btn => btn.addEventListener('click', () => {
  if (btn.dataset.tab === 'raw') loadRawConfig();
  if (btn.dataset.tab === 'sources') loadConfigSources();
}));
// If the page boots straight into one of these tabs (#raw / #sources), prime it.
if (location.hash === '#raw') loadRawConfig();
if (location.hash === '#sources') loadConfigSources();

// ────────────────────────────────────────────────────────────────
// Restart button
// ────────────────────────────────────────────────────────────────
document.getElementById('restart-btn').addEventListener('click', async () => {
  if (!confirm('Restart the Octo container? In-flight requests drop. Service comes back in 5-10s.')) return;
  const btn = document.getElementById('restart-btn');
  const label = btn.querySelector('span');
  btn.disabled = true;
  if (label) label.textContent = 'Restarting…';
  toast('Restart triggered. Waiting for service…');

  try { await fetch('/api/admin/restart', { method: 'POST' }); }
  catch { /* expected — connection drops */ }

  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 1500));
    try {
      const r = await fetch('/api/admin/status', { cache: 'no-store' });
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
// Boot
// ────────────────────────────────────────────────────────────────
loadSettings();
