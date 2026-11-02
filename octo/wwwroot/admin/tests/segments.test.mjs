// Runs the dashboard's segmented controls (the part of admin.js between `function buildSegments`
// and `function normalizeAddress`) against fake elements.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const start = source.indexOf('function buildSegments()');
const end = source.indexOf('function normalizeAddress');
assert.ok(start > 0 && end > start, 'the segments block is in admin.js');
const block = source.slice(start, end);

function load() {
  const thumb = { style: {} };
  const buttons = [];
  const select = {
    value: 'Custom',
    options: ['Flat', 'ByArtist', 'Organized', 'Custom'].map(value => ({ value, textContent: value })),
    getAttribute: () => null,
    dispatchEvent() {},
  };
  const seg = {
    dataset: { segFor: 'f-folder-structure' },
    setAttribute() {},
    closest: () => null,
    appendChild(button) { buttons.push(button); },
    querySelector: selector => (selector === '.seg-thumb' ? thumb : null),
    querySelectorAll: selector => (selector === 'button' ? buttons : []),
  };
  const observers = [];
  const context = {
    document: {
      getElementById: id => (id === 'f-folder-structure' ? select : null),
      querySelectorAll: selector => (selector === '.seg[data-seg-for]' ? [seg] : []),
      createElement: () => ({
        dataset: {},
        classList: { toggle() {} },
        setAttribute() {},
        addEventListener() {},
        offsetLeft: 0,
        offsetWidth: 0,
      }),
      fonts: { ready: new Promise(() => {}) },
    },
    window: { addEventListener() {}, ResizeObserver: true },
    ResizeObserver: class {
      constructor(callback) { this.callback = callback; observers.push(this); }
      observe(target) { this.target = target; }
    },
    requestAnimationFrame: () => 0,
    decoratePageHeaders() {},
    buildHints() {},
    labelSettingControls() {},
  };
  vm.createContext(context);
  vm.runInContext(block, context);
  return { thumb, buttons, observers, seg };
}

test('a segmented control is placed again when its size changes', () => {
  const { thumb, buttons, observers, seg } = load();
  assert.equal(buttons.length, 4);
  const observer = observers.find(o => o.target === seg);
  assert.ok(observer, 'the control is watched for size changes');

  // The web font arrives and the buttons take their real widths.
  buttons.forEach((button, index) => { button.offsetLeft = 4 + index * 80; button.offsetWidth = 76; });
  observer.callback();
  assert.equal(thumb.style.left, '244px');
  assert.equal(thumb.style.width, '76px');
});
