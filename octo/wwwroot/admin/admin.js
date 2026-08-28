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
  // Segmented thumbs can only be measured once their pane is visible.
  if (typeof syncSegments === 'function') syncSegments(false);
  if (name === 'fetched' && typeof loadFetched === 'function') loadFetched();
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
  const state = probe.warning ? 'warn' : probe.ok ? 'ok' : 'bad';
  card.classList.toggle('bad', state === 'bad');
  card.classList.toggle('warn', state === 'warn');
  const dot = card.querySelector('.status-dot');
  const body = card.querySelector('.status-card-body');
  if (dot)  dot.className  = `status-dot ${state}`;
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
    if (el.dataset.json === 'true') el.value = JSON.stringify(value ?? []);
    else if (el.type === 'checkbox') el.checked = !!value;
    else el.value = value;
  });

  syncPlaybackSourceControl();
  updateStreamSettings();
  updateRadioPublicationSettings();
  renderHeartSourceOrder(currentSettings?.Subsonic?.HeartDownloadSources);
  renderRadioDiscovery(currentSettings?.LastFm?.DiscoveryStations);
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

  updateDiscoveryBanner();
  buildSegments();
  syncSegments(false);
  if (currentSettings?.Lidarr?.BaseUrl && currentSettings?.Lidarr?.ApiKey) {
    loadLidarrOptions();
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
    ${form.id === 'lidarr-connection-form' ? '<button type="button" class="btn btn-ghost" id="lidarr-test-connection">Test connection</button>' : ''}
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
    if (form.id === 'radio-discovery-form' && !syncRadioDiscoveryInput()) return;
    const patch = {};
    let needsRestart = false;

    form.querySelectorAll('[name]').forEach(el => {
      if (!el.name?.includes('.')) return;
      if (el.disabled) return;
      const [section, key] = el.name.split('.');
      patch[section] = patch[section] || {};
      let value;
      if (el.dataset.json === 'true') {
        try { value = JSON.parse(el.value || '[]'); }
        catch { value = []; }
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

      // The JSON configuration provider reloads asynchronously after the atomic
      // write. Give it one watcher tick before testing a newly saved connection.
      if (form.id === 'lidarr-connection-form') await new Promise(resolve => setTimeout(resolve, 600));
      currentSettings = await (await fetch('/api/admin/settings')).json();
      if (form.id === 'lidarr-connection-form') await loadLidarrOptions();
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

document.querySelectorAll('[data-open-tab]').forEach(button => {
  button.addEventListener('click', () => activateTab(button.dataset.openTab));
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

document.getElementById('lidarr-test-connection')?.addEventListener('click', async (event) => {
  const button = event.currentTarget;
  const form = document.getElementById('lidarr-connection-form');
  const status = form?.querySelector('.saved-status');
  const baseUrl = document.getElementById('f-lidarr-url')?.value ?? '';
  const apiKey = document.getElementById('f-lidarr-key')?.value ?? '';
  button.disabled = true;
  if (status) status.textContent = 'Testing…';
  try {
    const r = await fetch('/api/admin/lidarr/test', {
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
    const r = await fetch('/api/admin/lidarr/options', { cache: 'no-store' });
    const data = await r.json();
    if (!r.ok) throw new Error(data.error || `HTTP ${r.status}`);

    populateLidarrOptions(data);
  } catch (err) {
    if (status) status.textContent = `Could not load choices: ${err.message}`;
  }
}

function populateLidarrOptions(data) {
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
  if (btn.dataset.tab === 'lastfm') loadRadioStatus();
}));
// If the page boots straight into one of these tabs, prime it.
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
    const response = await fetch(`/api/admin/lastfm/radio${query}`); if (!response.ok) throw new Error(`HTTP ${response.status}`);
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
  event.currentTarget.disabled = true;
  try { const response = await fetch(`/api/admin/lastfm/radio/history?user=${encodeURIComponent(user)}`, { method: 'DELETE' }); const data = await response.json();
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`); toast(data.message || 'Radio history reset.'); await loadRadioStatus();
  } catch (error) { toast(`Reset failed: ${error.message}`, 'error'); } finally { event.currentTarget.disabled = false; }
});

// ────────────────────────────────────────────────────────────────
// Notifications: send a test through every configured transport
// ────────────────────────────────────────────────────────────────
document.getElementById('btn-test-notification')?.addEventListener('click', async () => {
  const box = document.getElementById('test-notification-result');
  const btn = document.getElementById('btn-test-notification');
  if (!box) return;
  box.hidden = false;
  box.textContent = 'Sending…';
  btn.disabled = true;
  try {
    const r = await fetch('/api/admin/test-notification', { method: 'POST' });
    const d = await r.json();
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
    const r = await fetch('/api/admin/library-status', { cache: 'no-store' });
    if (!r.ok) return;
    const s = await r.json();
    effectiveLibraryPath = s.effectiveDownloadPath || '';

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
      pick.innerHTML = [`<option value="">First library Navidrome reports</option>`]
        .concat(libs.map(l => {
          const label = `${l.name || l.folder}${l.visible ? '' : ' (not visible to Octo)'}`;
          return `<option value="${esc(l.folder)}">${esc(label)}</option>`;
        })).join('');
      pick.value = s.pinnedLibraryPath || '';
      pickRow.hidden = false;
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
  return fetch(url, { cache: 'no-store', credentials: 'same-origin' });
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
      if (e.key === 'Enter') { e.preventDefault(); onSubmit(); }
      if (e.key === 'Escape') { e.preventDefault(); onCancel(); }
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
  if (!creds) return false;
  const r = await fetch('/api/admin/browse/auth', {
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
    const r = await fetch('/api/admin/discover-servers', { cache: 'no-store' });
    const data = await r.json();
    const servers = data.servers || [];
    if (!servers.length) {
      if (result) result.innerHTML =
        'No server found on this network. If Octo runs in a Docker bridge network it cannot see your LAN — use host networking, or enter the URL manually.';
    } else if (servers.length === 1) {
      urlInput.value = servers[0].url;
      urlInput.dispatchEvent(new Event('input', { bubbles: true }));
      if (result) result.innerHTML =
        `Found <strong>${servers[0].type || 'Subsonic'}</strong> ${servers[0].serverVersion || ''} at <code>${servers[0].url}</code> — filled in above. Save to apply.`;
    } else {
      const rows = servers.map(s =>
        `<button type="button" class="btn btn-ghost detect-pick" data-url="${s.url}">${s.url} <span class="detect-tag">${s.type || 'subsonic'} ${s.serverVersion || ''}</span></button>`).join('');
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
    const r = await fetch('/api/admin/downloads', { cache: 'no-store' });
    const data = await r.json();
    const items = data.downloads || [];
    if (!items.length) {
      list.innerHTML = '<div class="dl-empty">No downloads yet. Star a song in your music app and it will appear here.</div>';
      return;
    }
    list.innerHTML = items.map(d => {
      const fmt = (d.format || '?').toUpperCase();
      const badgeClass = fmt === 'FLAC' ? 'flac' : 'mp3';
      const art = d.coverArtUrl
        ? `<img class="dl-art" src="${escapeHtml(d.coverArtUrl)}" alt="" loading="lazy" onerror="this.replaceWith(Object.assign(document.createElement('div'),{className:'dl-art dl-art-ph'}))">`
        : `<div class="dl-art dl-art-ph"></div>`;
      const size = fmtSize(d.sizeBytes);
      return `<div class="dl-item">
        ${art}
        <div class="dl-main">
          <div class="dl-title">${escapeHtml(d.artist)} <span class="dl-dash">—</span> ${escapeHtml(d.title)}</div>
          <div class="dl-path" title="${escapeHtml(d.path)}">${escapeHtml(d.path)}</div>
        </div>
        <div class="dl-side">
          <div class="dl-tags"><span class="dl-badge ${badgeClass}">${escapeHtml(fmt)}</span><span class="dl-source">${escapeHtml(d.source)}</span></div>
          <div class="dl-sub">${escapeHtml(relTime(d.downloadedAt))}${size ? ' · ' + size : ''}</div>
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
buildSegments();
window.addEventListener('resize', () => syncSegments(false));

// ────────────────────────────────────────────────────────────────
// "Point your apps at Octo" address + copy
// ────────────────────────────────────────────────────────────────
(function initOctoAddress() {
  const el = document.getElementById('octo-address');
  if (!el) return;
  const here = new URL(location.href);
  const addr = `${here.protocol}//${here.hostname}:${here.port || '5274'}`;
  el.textContent = addr;
  document.getElementById('copy-octo-address')?.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(addr);
      if (typeof toast === 'function') toast('Address copied.', 'ok');
    } catch { /* clipboard blocked; user can select manually */ }
  });
})();

// ────────────────────────────────────────────────────────────────
// Boot
// ────────────────────────────────────────────────────────────────
loadSettings();
