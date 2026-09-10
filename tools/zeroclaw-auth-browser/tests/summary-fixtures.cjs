// Run the exact production summary program offline; no browser is launched.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/browser.rs'), 'utf8');
const script = source.match(/const SUMMARY_SCRIPT: &str = r#"([\s\S]*?)"#;/)[1];
const controls = Array.from({length: 15}, (_, i) => ({id: i, tagName:'BUTTON', text:`Control ${i}`}));
const run = offset => vm.runInNewContext(`(() => {${script}})()`, {
 controlsSelector: 'fixture', deepQuery: () => controls, visible: () => true,
 composedText: () => 'Confirmed fixture appointment', document: {body:{}},
 location: {href:'https://example.com/confirmation'}, args: {offset},
 controlText: e => e.text, disabled: () => false,
 selectorFor: e => {if (e.id === 1) throw new Error('Could not create a unique control selector'); return `#control${e.id}`;}
});
const first = run(0);
assert.equal(first.text, 'Confirmed fixture appointment');
assert.equal(first.totalControls, 15);
assert.equal(first.controls.length, 12);
assert.equal(first.controls[0].selector, '#control0');
assert.equal(first.controls[1].selector, null);
assert.match(first.controls[1].selector_error, /unique control selector/);
assert.equal(first.controls[1].text, 'Control 1');
assert.equal(first.controls[2].selector, '#control2');
const last = run(12);
assert.equal(last.controls.length, 3);
assert.equal(last.controls[0].selector, '#control12');
console.log('Summary fixture passed: one unsupported control preserves page text, other controls, and offsets.');
