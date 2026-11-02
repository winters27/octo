// Runs the dashboard's Tidy card (the part of admin.js between `let lastTidyRun` and
// `// ---- Mark explicit songs`) against a fake page and a scripted API.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const start = source.indexOf('let lastTidyRun');
const end = source.indexOf('// ---- Mark explicit songs');
assert.ok(start > 0 && end > start, 'the Tidy block is in admin.js');
const block = source.slice(start, end);

function element(extra = {}) {
  const handlers = {};
  return {
    value: '',
    hidden: false,
    disabled: false,
    innerHTML: '',
    textContent: '',
    dataset: {},
    handlers,
    addEventListener(type, handler) { handlers[type] = handler; },
    querySelector: () => null,
    ...extra,
  };
}

const escape = text => String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

const option = (value, task) => ({ value, hidden: false, dataset: task ? { task } : {} });

function load({ replies = [], links = [], confirmed = false } = {}) {
  const kind = element({
    value: 'Changes',
    options: [
      option('Changes'),
      option('Split', 'Artists'),
      option('Respell', 'Artists'),
      option('Rename', 'Artists'),
      option('Guest', 'Artists'),
      option('Layout', 'FileLayout'),
      option('Skip'),
    ],
  });
  const byId = {
    'library-tidy-task': element({ value: 'Artists' }),
    'library-tidy-task-hint': element({ hidden: true }),
    'library-tidy-scope': element({ value: 'OctoDownloads' }),
    'library-tidy-scope-hint': element({ hidden: true }),
    'library-tidy-excluded': element(),
    'library-tidy-desc': element(),
    'library-tidy-notice': element(),
    'library-tidy-preview-note': element(),
    'library-tidy-status': element(),
    'library-tidy-apply-note': element(),
    'library-tidy-apply': element({ hidden: true }),
    'library-tidy-resume': element({ hidden: true }),
    'library-tidy-cancel': element({ hidden: true }),
    'library-tidy-undo': element({ hidden: true }),
    'library-tidy-preview': element(),
    'library-tidy-filter': element({ hidden: true }),
    'library-tidy-kind': kind,
    'library-tidy-results': element(),
    'library-tidy-pager': element({ hidden: true }),
    'library-tidy-page-label': element(),
    'library-tidy-prev': element(),
    'library-tidy-next': element(),
    'library-tidy': element({ scrollIntoView() {} }),
  };
  const asked = [];
  const confirms = [];
  const opened = [];
  const context = {
    document: {
      getElementById: id => byId[id] ?? null,
      querySelectorAll: selector => (selector === '[data-tidy-task]' ? links : []),
      querySelector: () => null,
    },
    window: { matchMedia: () => ({ matches: true }) },
    api: async (url, options = {}) => {
      asked.push({ url, body: options.body ? JSON.parse(options.body) : null });
      const reply = replies.shift() ?? (() => ({ status: 'Idle' }));
      return { ok: true, status: 200, json: async () => reply(url) };
    },
    browseAuthenticate: async () => false,
    note() {},
    esc: escape,
    askConfirm: async (title, message, confirm) => { confirms.push({ title, message, confirm }); return confirmed; },
    askDialog: async () => '',
    syncSegment() {},
    openTab: name => opened.push(name),
    requestAnimationFrame: callback => callback(),
    setInterval: () => 1,
    clearInterval() {},
  };
  vm.createContext(context);
  const functions = vm.runInContext(
    `let libraryTidyPoll = null;\n${block}\n;({ tidyCounts, tidyTable, loadTidy, renderLibraryTidy, showTidyTask, chooseTidyTask, tidyWords })`,
    context,
  );
  const set = (name, value) => vm.runInContext(`${name} = ${JSON.stringify(value)}`, context);
  return { byId, asked, confirms, opened, set, ...functions };
}

const previewed = extra => ({
  status: 'Completed',
  mode: 'Preview',
  scope: 'OctoDownloads',
  task: 'Artists',
  previewed: true,
  applyAllowed: true,
  rows: { Respell: 1 },
  files: 3,
  plannedRetags: 1,
  plannedMoves: 1,
  ...extra,
});

test('the File layout task counts folders and songs, the Artists task as before', () => {
  const page = load();
  const layout = { task: 'FileLayout', files: 9, plannedMoves: 2, plannedFileMoves: 5, rows: { Skip: 1 } };
  assert.equal(page.tidyCounts({ ...layout, mode: 'Preview' }), '9 files read · 2 folders to move · 5 songs to move · 1 left alone');
  assert.equal(
    page.tidyCounts({ task: 'FileLayout', mode: 'Apply', moved: 2, movedFiles: 5, held: 1 }),
    '2 folders moved · 5 songs moved · 1 held back',
  );
  assert.equal(
    page.tidyCounts({ task: 'FileLayout', mode: 'Undo', moved: 1, movedFiles: 4 }),
    '1 folder moved back · 4 songs moved back',
  );
  assert.equal(
    page.tidyCounts({ task: 'Artists', mode: 'Preview', files: 3, plannedRetags: 1, plannedMoves: 2 }),
    '3 files read · 1 to re-tag · 2 album folders to move',
  );
});

test('a count of 1 reads singular', () => {
  const page = load();
  assert.equal(
    page.tidyCounts({ task: 'FileLayout', mode: 'Preview', files: 1, plannedMoves: 1, plannedFileMoves: 1 }),
    '1 file read · 1 folder to move · 1 song to move',
  );
  assert.equal(
    page.tidyCounts({ task: 'FileLayout', mode: 'Apply', moved: 1, movedFiles: 1 }),
    '1 folder moved · 1 song moved',
  );
});

test('a finished File layout apply with no preview shows the File layout task', async () => {
  const run = { status: 'Completed', mode: 'Apply', task: 'FileLayout', previewed: false, moved: 1, movedFiles: 2 };
  const page = load({ replies: [() => run] });
  await page.loadTidy();
  assert.equal(page.byId['library-tidy-task'].value, 'FileLayout');
});

test('a running run says Cancel stops it after the current file', () => {
  const page = load();
  page.renderLibraryTidy({ status: 'Running', mode: 'Apply', task: 'FileLayout', phase: 'Moving' });
  assert.equal(page.byId['library-tidy-apply-note'].textContent, 'Cancel stops it after the current file.');
});

test('a Layout row reads Into the layout and leaves out its whole folder', () => {
  const page = load();
  const html = page.tidyTable([
    { id: 'layout|Daft Punk/Discovery', kind: 'Layout', path: 'Daft Punk/Discovery', before: 'Daft Punk/Discovery', after: 'Daft Punk/Discovery (2001)', evidence: '12 songs move into the layout, for example 01 - One More Time.flac' },
    { id: 'layout|', kind: 'Layout', path: '', before: '', after: 'Daft Punk/Discovery', evidence: '2 songs move into the layout, for example 01 - One More Time.flac' },
    { id: 'split|A, B', kind: 'Split', path: 'A, B/Album', before: 'A, B', after: 'A', evidence: '' },
  ], true);
  assert.ok(html.includes('>Into the layout</span>'), 'a Layout row names only its kind');
  assert.ok(html.includes('data-label="Before">Daft Punk/<wbr>Discovery</span>'), 'a path breaks after its slashes');
  assert.ok(html.includes('tidy-table layout'), 'a table with a Layout row uses the layout columns');
  assert.ok(html.includes('Folder split: A, B/<wbr>Album'), 'an artist row names its path');
  assert.ok(html.includes('data-tidy-exclude="Daft Punk/Discovery"'));
  assert.ok(html.includes('The music folder'), 'the music folder row names it');
  assert.ok(html.includes('data-tidy-exclude="A, B"'), 'an artist row still leaves out its artist folder');
  assert.ok(!html.includes('class="act"'), 'the Leave out cell never takes the activity card class');
});

test('a row of members\' songs reads as theirs and leaves out its pool folder', () => {
  const page = load();
  const html = page.tidyTable([
    { id: 'pool|Artist/Album', kind: 'Layout', path: 'pool:Artist/Album', before: 'Artist/Album', after: 'Artist/Album (2004)', evidence: '1 song moves into the layout, for example 01 - Song.flac' },
  ], true);
  assert.ok(html.includes("Into the layout, members' songs"));
  assert.ok(!html.includes('Into the layout: pool:'));
  assert.ok(html.includes('data-tidy-exclude="pool:Artist/Album"'));
});

test('switching the task after a preview says to preview again and keeps Apply off', () => {
  const page = load();
  page.byId['library-tidy-task'].value = 'FileLayout';
  page.renderLibraryTidy(previewed());
  assert.equal(page.byId['library-tidy-task-hint'].hidden, false);
  assert.equal(page.byId['library-tidy-apply'].hidden, true);

  page.byId['library-tidy-task'].value = 'Artists';
  page.renderLibraryTidy(previewed());
  assert.equal(page.byId['library-tidy-task-hint'].hidden, true);
  assert.equal(page.byId['library-tidy-apply'].hidden, false);
});

test('a File layout preview made with another layout keeps Apply off and says why', () => {
  const page = load();
  page.byId['library-tidy-task'].value = 'FileLayout';
  const why = 'The File layout changed since the preview. Preview again.';
  page.renderLibraryTidy(previewed({ task: 'FileLayout', rows: { Layout: 2 }, layoutRefusal: why }));
  assert.equal(page.byId['library-tidy-apply'].hidden, true);
  assert.equal(page.byId['library-tidy-apply-note'].textContent, why);
});

test('the card says what the chosen task does, and offers its kinds', () => {
  const page = load();
  const kinds = () => Object.fromEntries(page.byId['library-tidy-kind'].options.map(o => [o.value, o.hidden]));
  page.showTidyTask('FileLayout');
  assert.equal(page.byId['library-tidy-desc'].innerHTML, page.tidyWords('FileLayout').desc);
  assert.equal(page.byId['library-tidy-notice'].innerHTML, page.tidyWords('FileLayout').notice);
  assert.equal(page.byId['library-tidy-preview-note'].innerHTML, page.tidyWords('FileLayout').preview);
  assert.deepEqual(kinds(), { Changes: false, Split: true, Respell: true, Rename: true, Guest: true, Layout: false, Skip: false });

  page.byId['library-tidy-kind'].value = 'Layout';
  page.showTidyTask('Artists');
  assert.equal(page.byId['library-tidy-desc'].innerHTML, page.tidyWords('Artists').desc);
  assert.equal(kinds().Layout, true);
  assert.equal(page.byId['library-tidy-kind'].value, 'Changes', 'a kind the task does not have is not kept');
});

test('the File layout card link opens this card with File layout chosen', () => {
  const link = element({ dataset: { tidyTask: 'FileLayout' } });
  const page = load({ links: [link] });
  let prevented = false;
  link.handlers.click({ preventDefault: () => { prevented = true; } });
  assert.ok(prevented, 'the link does not follow its hash');
  assert.equal(page.byId['library-tidy-task'].value, 'FileLayout');
  assert.deepEqual(page.opened, ['tags']);
  assert.equal(page.byId['library-tidy-desc'].innerHTML, page.tidyWords('FileLayout').desc);
});

test('a preview asks for the chosen task', async () => {
  const page = load();
  page.byId['library-tidy-task'].value = 'FileLayout';
  await page.byId['library-tidy-preview'].handlers.click();
  const preview = page.asked.find(call => call.url === '/api/admin/tidy/preview');
  assert.equal(preview.body.task, 'FileLayout');
});

test('undo says what it puts back from the task of the run it undoes', async () => {
  const page = load();
  page.set('lastTidyRun', { status: 'Completed', mode: 'Apply', task: 'Artists', undoTask: 'FileLayout' });
  await page.byId['library-tidy-undo'].handlers.click();
  assert.equal(page.confirms[0].message, page.tidyWords('FileLayout').undoMessage);
});

test('undo puts back the last tidy and sends the run the page showed', async () => {
  const run = {
    status: 'Completed', mode: 'Apply', task: 'FileLayout', canUndo: true,
    undoTask: 'FileLayout', undoRun: 'r2', rows: {},
  };
  const page = load({ confirmed: true });
  page.renderLibraryTidy(run);
  assert.equal(page.byId['library-tidy-undo'].hidden, false);
  assert.equal(page.byId['library-tidy-apply-note'].textContent,
    'Once a preview has finished, for the same files it previewed.');
  page.set('lastTidyRun', run);
  await page.byId['library-tidy-undo'].handlers.click();
  assert.equal(page.confirms[0].title, 'Undo the last tidy?');
  assert.equal(page.confirms[0].message, page.tidyWords('FileLayout').undoMessage);
  const undo = page.asked.find(call => call.url === '/api/admin/tidy/undo');
  assert.deepEqual(undo.body, { run: 'r2' });
});

test('apply asks with the words and the task of the preview', async () => {
  const run = { ...previewed({ task: 'FileLayout', rows: { Layout: 2 } }) };
  const page = load({ replies: [() => run, () => ({ total: 0, offset: 0, rows: [] })] });
  page.byId['library-tidy-task'].value = 'FileLayout';
  await page.byId['library-tidy-apply'].handlers.click();
  assert.equal(page.confirms[0].title, page.tidyWords('FileLayout').applyTitle);
  assert.equal(page.confirms[0].confirm, 'Move');
});
