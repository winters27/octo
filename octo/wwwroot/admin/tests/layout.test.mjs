// Runs the dashboard's File layout block (the part of admin.js between `// ---- File layout` and
// `let lastTidyRun`) against a scripted fake of the layout preview.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const start = source.indexOf('// ---- File layout');
const end = source.indexOf('let lastTidyRun');
assert.ok(start > 0 && end > start, 'the File layout block is in admin.js');
const block = source.slice(start, end);

function element(extra = {}) {
  return {
    value: '',
    hidden: false,
    innerHTML: '',
    textContent: '',
    attributes: {},
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
    addEventListener() {},
    ...extra,
  };
}

const escape = text => String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// Each call to the preview takes the next reply; a reply may hold a promise to answer later.
function load(replies = []) {
  const byId = {
    'f-folder-structure': element({ value: 'Custom' }),
    'f-folder-template': element({ value: '{Artist}/{Album}/{Track:00} - {Title}' }),
    'f-folder-template-singles': element(),
    'f-folder-template-compilations': element(),
    'f-folder-template-error': element({ hidden: true }),
    'f-folder-template-singles-error': element({ hidden: true }),
    'f-folder-template-compilations-error': element({ hidden: true }),
    'layout-examples': element(),
    'layout-fallback': element({ hidden: true }),
    'layout-caps-problem': element({ hidden: true }),
    'layout-names': element(),
  };
  const timers = [];
  const context = {
    document: {
      getElementById: id => byId[id] ?? null,
      querySelectorAll: () => [],
      activeElement: null,
    },
    api: async (url, options = {}) => {
      const body = JSON.parse(options.body);
      const reply = replies.shift();
      const answer = await reply(body);
      return { ok: true, status: 200, json: async () => answer };
    },
    esc: escape,
    setTimeout: (callback, ms) => { timers.push(ms); return timers.length; },
    clearTimeout() {},
  };
  vm.createContext(context);
  const functions = vm.runInContext(
    `${block}\n;({ showLayoutExamples, showLayoutFallback, showLayoutCapsProblem, loadLayoutPreview })`,
    context,
  );
  // The block's own `let`s live on in the context, so a test can say when the person last typed.
  const typedAt = when => vm.runInContext(`layoutTypedAt = ${when}`, context);
  return { byId, context, timers, typedAt, ...functions };
}

const examples = [
  { kind: 'Album', label: 'Album track', path: 'Daft Punk/Discovery/01 - One More Time.flac', field: 'FolderTemplate' },
  { kind: 'Single', label: 'Single', path: 'Billie Eilish/Singles/What Was I Made For.flac', field: 'FolderTemplateSingles' },
  { kind: 'Compilation', label: 'Compilation track', path: 'Various/Now 115/03 - Flowers.mp3', field: 'FolderTemplate' },
];

const count = (text, part) => text.split(part).length - 1;
// A path may break after each `/`; the tests read it without those break points.
const text = holder => holder.innerHTML.replaceAll('<wbr>', '');

test('an example whose template is broken says which field to fix, the others show their path', () => {
  const page = load();
  page.showLayoutExamples(examples, null, [{ field: 'FolderTemplateSingles', message: 'Leave out .flac' }]);
  const html = text(page.byId['layout-examples']);
  assert.equal(count(html, 'Shown once the Singles layout is fixed.'), 1);
  assert.ok(html.includes('Daft Punk/Discovery/01 - One More Time.flac'));
  assert.ok(html.includes('Various/Now 115/03 - Flowers.mp3'));
  assert.ok(!html.includes('What Was I Made For.flac'), 'the stand-in path is never shown');
});

test('when every example waits on a fix it is said once', () => {
  const page = load();
  const all = examples.map(example => ({ ...example, field: 'FolderTemplate' }));
  page.showLayoutExamples(all, null, [{ field: 'FolderTemplate', message: '{Artst} is not a name' }]);
  const html = text(page.byId['layout-examples']);
  assert.equal(count(html, 'Shown once the Albums layout is fixed.'), 1);
  assert.ok(!html.includes('layout-example-k'), 'no per-song rows');
});

test('the saved-layout notice comes only from the on-load answer and goes once a saved layout is whole', async () => {
  const broken = { field: 'FolderTemplate', message: '{Artst} is not a name' };
  const answer = problems => body => ({ seq: body.seq, problems, examples });
  const page = load([answer([broken]), answer([]), answer([])]);
  const fallback = page.byId['layout-fallback'];

  await page.loadLayoutPreview(true);
  assert.equal(fallback.hidden, false);
  assert.equal(
    fallback.innerHTML,
    'The saved <strong>Albums</strong> layout has a problem, so new songs are filed as Organized until it is fixed.',
  );

  // Typing a good template does not change what is saved, so the notice stays.
  await page.loadLayoutPreview();
  assert.equal(fallback.hidden, false);

  // Settings loaded again after a save with a whole layout.
  await page.loadLayoutPreview(true);
  assert.equal(fallback.hidden, true);
});

test('the card shows the server line when layout-caps.json is not saved, as text, and hides it on null', () => {
  const page = load();
  const holder = page.byId['layout-caps-problem'];
  const line = 'Octo could not save /app/config/layout-caps.json, which remembers how older layouts shortened long folder names: Permission denied <os error 13>. It tries again each time the settings are read.';

  page.showLayoutCapsProblem(line);
  assert.equal(holder.hidden, false);
  assert.equal(holder.textContent, line);
  assert.equal(holder.innerHTML, '', 'the line is set as text, never as markup');

  page.showLayoutCapsProblem(null);
  assert.equal(holder.hidden, true);
  assert.equal(holder.textContent, '');

  page.showLayoutCapsProblem(line);
  page.showLayoutCapsProblem(undefined);
  assert.equal(holder.hidden, true, 'an answer without the field hides it too');
});

test('typing before the on-load answer arrives still shows the saved-layout notice', async () => {
  const broken = { field: 'FolderTemplate', message: '{Artst} is not a name' };
  let answerOnLoad;
  const onLoad = body => new Promise(resolve => {
    answerOnLoad = () => resolve({ seq: body.seq, problems: [broken], examples });
  });
  const typed = body => ({ seq: body.seq, problems: [], examples: [{ ...examples[0], path: 'typed/path.flac', field: null }] });
  const page = load([onLoad, typed]);
  const fallback = page.byId['layout-fallback'];

  const loading = page.loadLayoutPreview(true);
  await page.loadLayoutPreview();
  answerOnLoad();
  await loading;

  assert.equal(fallback.hidden, false);
  assert.equal(
    fallback.innerHTML,
    'The saved <strong>Albums</strong> layout has a problem, so new songs are filed as Organized until it is fixed.',
  );
  // The examples and the field's own words stay those of what was typed last.
  const html = text(page.byId['layout-examples']);
  assert.ok(html.includes('typed/path.flac'));
  assert.equal(page.byId['f-folder-template-error'].hidden, true);
});

test('an older saved answer never replaces the notice of a newer one', async () => {
  const broken = { field: 'FolderTemplate', message: '{Artst} is not a name' };
  let answerFirst;
  const first = body => new Promise(resolve => {
    answerFirst = () => resolve({ seq: body.seq, problems: [broken], examples });
  });
  const second = body => ({ seq: body.seq, problems: [], examples });
  const page = load([first, second]);
  const fallback = page.byId['layout-fallback'];

  const older = page.loadLayoutPreview(true);
  await page.loadLayoutPreview(true);
  answerFirst();
  await older;
  assert.equal(fallback.hidden, true);
});

test('a new mistake in the field being typed waits for a pause, and shows once the field is left', async () => {
  const broken = { field: 'FolderTemplate', message: '{Alb} is not a name' };
  const answer = body => ({ seq: body.seq, problems: [broken], examples });
  const page = load([answer, answer]);
  const field = page.byId['f-folder-template'];
  field.id = 'f-folder-template';
  field.classList = { contains: name => name === 'layout-template' };
  page.context.document.activeElement = field;
  const error = page.byId['f-folder-template-error'];

  page.typedAt('Date.now()');
  await page.loadLayoutPreview();
  assert.equal(error.hidden, true, 'not while typing');
  assert.equal(page.timers.length, 1, 'asks again after the pause');
  assert.ok(page.timers[0] > 0 && page.timers[0] <= 800);

  // Leaving the field (or the pause) shows it.
  page.typedAt(0);
  await page.loadLayoutPreview();
  assert.equal(error.hidden, false);
  assert.equal(error.textContent, '{Alb} is not a name');
});

test('a name in braces in a mistake never breaks across lines', () => {
  const page = load();
  page.showLayoutExamples(examples, null, []);
  const field = { field: 'FolderTemplate', message: 'Use one of {Album Type} or {Track Artist}.' };
  vm.runInContext('showLayoutProblems', page.context)([field]);
  assert.equal(page.byId['f-folder-template-error'].textContent, 'Use one of {Album Type} or {Track Artist}.');
});

test('an older answer is never drawn over a newer one', async () => {
  let answerFirst;
  const first = body => new Promise(resolve => {
    answerFirst = () => resolve({ seq: body.seq, problems: [], examples: [{ ...examples[0], path: 'old/path.flac', field: null }] });
  });
  const second = body => ({ seq: body.seq, problems: [], examples: [{ ...examples[0], path: 'new/path.flac', field: null }] });
  const page = load([first, second]);
  const older = page.loadLayoutPreview();
  await page.loadLayoutPreview();
  answerFirst();
  await older;
  const html = text(page.byId['layout-examples']);
  assert.ok(html.includes('new/path.flac'));
  assert.ok(!html.includes('old/path.flac'));
});
