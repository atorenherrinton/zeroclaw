// Execute the exact embedded reader/action programs offline. No browser or
// AppleScript is launched. The small DOM fixture models only their API boundary.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const common = fs.readFileSync(path.join(__dirname, '../src/dom/common.js'), 'utf8');
const program = name => common + fs.readFileSync(path.join(__dirname, `../src/dom/${name}.js`), 'utf8');
const actionProgram = program('interact');
const readProgram = `(() => {${program('read')}})()`;
const inspectProgram = program('inspect');
const readyProgram = program('ready');
class Element {
  constructor(tag, attrs = {}) {
    this.tagName = tag.toUpperCase(); this.attrs = attrs; this.nodeType = 1;
    this.children = []; this.parentElement = null; this.events = [];
    this.innerText = ''; this.labels = []; this.style = {};
    this.rect = {top: 10, left: 10, bottom: 30, right: 200};
    this.disabled = false; this.readOnly = false; this.clicks = 0;
  }
  append(child) { this.children.push(child); child.parentElement = this; return child; }
  get id() { return this.attrs.id || ''; }
  getAttribute(name) { return this.attrs[name] ?? null; }
  getClientRects() { return this.hidden ? [] : [this.rect]; }
  getBoundingClientRect() { return this.rect; }
  closest() { for (let node = this; node; node = node.parentElement) { if ('hidden' in node.attrs || node.attrs['aria-hidden'] === 'true') return node; } return null; }
  matches(selector) { assert.equal(selector, ':disabled'); return this.disabled; }
  dispatchEvent(event) { this.events.push(event.type); return true; }
  querySelectorAll(selector) { return allNodes(this).slice(1).filter(node => selectorMatches(node, selector)); }
  focus() { this.focused = true; }
  blur() { this.focused = false; }
  click() { this.clicks++; if (this.type === 'checkbox' && !this.preventClick) this.checked = !this.checked; }
}
class HTMLInputElement extends Element {
  constructor(attrs = {}) { super('input', attrs); this.type = attrs.type || 'text'; this._value = ''; this.checked = false; }
  cloneNode() { return new HTMLInputElement({...this.attrs}); }
  get validity() { return {valid: !this.invalid, valueMissing: false}; }
  get value() { return this._value; }
  set value(value) { this._value = String(value); }
}
class HTMLTextAreaElement extends Element {
  constructor(attrs = {}) { super('textarea', attrs); this._value = ''; }
  get value() { return this._value; }
  set value(value) { this._value = String(value); }
}
class HTMLSelectElement extends Element {
  constructor(options, attrs = {}) { super('select', attrs); this.options = options; this.multiple = false; }
  get value() { return this.options.find(option => option.selected)?.value || ''; }
  set value(value) { for (const option of this.options) option.selected = option.value === value; }
}
function option(value, label, disabled = false) { return {value, label, disabled, selected: false, parentElement: null}; }
function allNodes(root) { return [root, ...root.children.flatMap(allNodes)]; }
function partMatches(node, part) {
  const match = part.match(/^([a-z]+)(?::nth-of-type\((\d+)\))?$/);
  if (!match || node.tagName.toLowerCase() !== match[1]) return false;
  if (!match[2]) return true;
  const siblings = node.parentElement.children.filter(sibling => sibling.tagName === node.tagName);
  return siblings.indexOf(node) + 1 === Number(match[2]);
}
function selectorMatches(node, selector) {
  const attr = selector.match(/^\[([\w-]+)="([^"]+)"\]$/);
  if (attr) return node.getAttribute(attr[1]) === attr[2];
  if (selector.startsWith('#')) return node.id === selector.slice(1);
  const parts = selector.split(' > ').reverse();
  for (const part of parts) { if (!node || !partMatches(node, part)) return false; node = node.parentElement; }
  return true;
}
function context(controls, root) {
  const nodes = root ? allNodes(root) : controls;
  return {
    HTMLInputElement, HTMLTextAreaElement, HTMLSelectElement, Event, URL, setTimeout, clearTimeout,
    getComputedStyle: element => element.style,
    CSS: {escape: text => text},
    innerWidth: 1000, innerHeight: 800, scrollX: 0, scrollY: 0,
    location: {href: 'https://example.com/form'},
    document: {
      readyState: 'complete', title: 'Offline form', body: {innerText: 'Fixture page'},
      querySelectorAll(selector) {
        if (selector.startsWith('a,button,input:')) return controls;
        return nodes.filter(node => selectorMatches(node, selector));
      }
    }
  };
}
function act(element, action, values = {}, matches = [element]) {
  return vm.runInNewContext(`(() => { const args = ${JSON.stringify({action, selector: '#field', text: '', key: '', ...values})}; ${actionProgram} })()`, context(matches));
}
function field(type = 'text') { return new HTMLInputElement({id: 'field', type}); }
let assertions = 0;
const pending = [];
function test(name, body) { pending.push([name, body]); }
function inspect(element, expected, nodes = [element]) { return JSON.parse(vm.runInNewContext(`(() => { const args = ${JSON.stringify({selector:'#field', ...expected})}; ${inspectProgram} })()`, context(nodes))); }
function ready(ctx, args = {}) { return JSON.parse(vm.runInNewContext(`(() => { const args = ${JSON.stringify(args)}; ${readyProgram} })()`, ctx)); }

test('fill bypasses a framework-owned value setter and notifies input/change', () => {
  const input = field(); let frameworkSetterCalls = 0;
  Object.defineProperty(input, 'value', {get() { return this._value; }, set() { frameworkSetterCalls++; }});
  act(input, 'fill', {text: 'Test User'});
  assert.equal(input.value, 'Test User'); assert.equal(frameworkSetterCalls, 0);
  assert.deepEqual(input.events, ['input', 'change']);
  const textarea = new HTMLTextAreaElement({id: 'field'});
  act(textarea, 'fill', {text: 'Line one\nLine two'});
  assert.equal(textarea.value, 'Line one\nLine two');
});
test('select uses exact values and rejects disabled, duplicate, missing, multiple options', () => {
  const select = new HTMLSelectElement([option('one', 'First'), option('two', 'Second')], {id: 'field'});
  act(select, 'select', {text: 'two'}); assert.equal(select.value, 'two');
  assert.deepEqual(select.events, ['input', 'change']);
  select.options[0].disabled = true;
  assert.throws(() => act(select, 'select', {text: 'one'}), /Option is disabled/);
  assert.throws(() => act(select, 'select', {text: 'missing'}), /exactly one/);
  select.options.push(option('two', 'Duplicate'));
  assert.throws(() => act(select, 'select', {text: 'two'}), /exactly one/);
  select.multiple = true;
  assert.throws(() => act(select, 'select', {text: 'two'}), /single-selection/);
});
test('fill and select detect values reverted synchronously by framework handlers', () => {
  const input = field();
  input.dispatchEvent = function(event) { this.events.push(event.type); if (event.type === 'input') this._value = ''; return true; };
  assert.throws(() => act(input, 'fill', {text: 'new value'}), /did not retain the filled value/);
  const select = new HTMLSelectElement([option('one', 'First'), option('two', 'Second')], {id: 'field'});
  select.dispatchEvent = function(event) {
    this.events.push(event.type);
    if (event.type === 'change') Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(this, 'one');
    return true;
  };
  assert.throws(() => act(select, 'select', {text: 'two'}), /did not retain the selected option/);
});
test('check/uncheck are idempotent and verify the resulting state', () => {
  const input = field('checkbox'); act(input, 'check'); act(input, 'check');
  assert.equal(input.checked, true); assert.equal(input.clicks, 1);
  act(input, 'uncheck'); act(input, 'uncheck');
  assert.equal(input.checked, false); assert.equal(input.clicks, 2);
  input.preventClick = true;
  assert.throws(() => act(input, 'check'), /did not reach/);
  assert.throws(() => act(field('radio'), 'uncheck'), /checkbox input/);
});
test('ambiguous, invisible, disabled, readonly, and wrong-kind targets fail before writes', () => {
  const input = field();
  assert.throws(() => act(input, 'fill', {text: 'x'}, [input, field()]), /multiple elements/);
  input.hidden = true; assert.throws(() => act(input, 'click'), /not visible/);
  input.hidden = false; input.disabled = true; assert.throws(() => act(input, 'click'), /disabled/);
  input.disabled = false; input.readOnly = true; assert.throws(() => act(input, 'fill'), /read-only/);
  assert.throws(() => act(field('file'), 'fill'), /Fill requires/);
  assert.equal(input.value, ''); assert.equal(input.clicks, 0);
});
test('Enter activates links/buttons but never implicitly submits text inputs or comboboxes', () => {
  const input = field(); let submissions = 0;
  input.form = {requestSubmit() { submissions++; }};
  assert.throws(() => act(input, 'press', {key: 'Enter'}), /Enter only activates/);
  input.attrs.role = 'combobox';
  assert.throws(() => act(input, 'press', {key: 'Enter'}), /combobox is not form submission/);
  assert.equal(submissions, 0);
  const button = new Element('button', {id: 'field'});
  act(button, 'press', {key: 'Enter'}); assert.equal(button.clicks, 1);
  assert.throws(() => act(input, 'press', {key: 'Tab'}), /native Safari computer controls/);
  assert.throws(() => act(new HTMLTextAreaElement({id: 'field'}), 'press', {key: 'Enter'}), /native Safari Computer controls/);
});
test('reader generates unique selectors despite duplicate IDs/names beyond six ancestors', () => {
  const root = new Element('html'); const body = root.append(new Element('body'));
  const controls = [];
  for (let branch = 0; branch < 2; branch++) {
    let parent = body.append(new Element('section'));
    for (let depth = 0; depth < 8; depth++) parent = parent.append(new Element('div'));
    controls.push(parent.append(new HTMLInputElement({id: 'duplicate', name: 'same'})));
  }
  const ctx = context(controls, root);
  const page = JSON.parse(vm.runInNewContext(readProgram, ctx));
  assert.equal(page.controls.length, 2);
  for (let i = 0; i < controls.length; i++) {
    const matches = ctx.document.querySelectorAll(page.controls[i].selector);
    assert.equal(matches.length, 1); assert.equal(matches[0], controls[i]);
  }
  assert.notEqual(page.controls[0].selector, page.controls[1].selector);
});
test('reader omits hidden controls, prioritizes viewport controls, reports options/truncation without form values', () => {
  const controls = [];
  for (let i = 0; i < 161; i++) {
    const input = new HTMLInputElement({id: `item${i}`});
    input.rect = {top: 1000, bottom: 1020, left: 10, right: 200}; controls.push(input);
  }
  const hidden = field(); hidden.hidden = true; controls.push(hidden);
  const selected = option('two', 'Second'); selected.selected = true;
  const select = new HTMLSelectElement([option('one', 'First', true), selected], {id: 'onscreen'});
  select.disabled = true; controls.push(select);
  const password = new HTMLInputElement({id: 'secret', type: 'password'}); password.value = 'secret-fixture-value'; controls.push(password);
  const result = vm.runInNewContext(readProgram, context(controls));
  const page = JSON.parse(result);
  assert.equal(page.totalControls, 163); assert.equal(page.controls.length, 160);
  assert.equal(page.controlsTruncated, true); assert.equal(page.controls[0].selector, '#onscreen');
  assert.equal(page.controls[0].disabled, true); assert.equal(page.controls[0].inViewport, true);
  assert.equal(page.controls[0].options[0].disabled, true); assert.equal(page.controls[0].options[1].selected, true);
  assert.equal(page.controls[1].hasValue, true); assert.equal(page.controls[1].sensitive, true);
  assert.equal(result.includes('secret-fixture-value'), false);
  assert.equal(page.controls.some(control => control.selector === '#field'), false);
});
test('reader bounds large option lists and explicitly omits overlarge exact values', () => {
  const options = Array.from({length: 10000}, (_, i) => option(`value-${i}`, `Option ${i}`));
  const first = new HTMLSelectElement(options, {id: 'large'});
  const second = new HTMLSelectElement(options, {id: 'also-large'});
  const result = vm.runInNewContext(readProgram, context([first, second]));
  const page = JSON.parse(result);
  assert.ok(Buffer.byteLength(result) < 20 * 1024);
  for (const control of page.controls) {
    assert.equal(control.totalOptions, 10000); assert.equal(control.optionsTruncated, true);
    assert.ok(control.options.length <= 250);
  }
  const hugeValue = 'x'.repeat(2000);
  const huge = new HTMLSelectElement([option(hugeValue, 'y'.repeat(500))], {id: 'huge-value'});
  const hugePage = JSON.parse(vm.runInNewContext(readProgram, context([huge])));
  const item = hugePage.controls[0].options[0];
  assert.equal(item.valueOmitted, true); assert.equal('value' in item, false);
  assert.equal(item.label.length, 240); assert.equal(item.labelTruncated, true);
  assert.equal(hugePage.controls[0].totalOptions, 1);
});
test('ARIA selection enumerates only owned options and never commits typed search text', () => {
  const combo = field(); combo.attrs.role = 'combobox'; combo.attrs['aria-controls'] = 'popup';
  const popup = new Element('div', {id:'popup',role:'listbox'});
  const brown = popup.append(new Element('div',{id:'brown',role:'option','aria-selected':'false'})); brown.innerText = 'Brown';
  const disabledOption = popup.append(new Element('div',{id:'disabled',role:'option','aria-disabled':'true'})); disabledOption.innerText = 'Disabled';
  const unrelated = new Element('div',{id:'unrelated',role:'option'}); unrelated.innerText = 'Brown';
  const nodes = [combo,popup,brown,disabledOption,unrelated];
  combo.value = 'Brown';
  assert.equal(inspect(combo,{text:'Brown'},nodes).matches,false);
  const read = JSON.parse(vm.runInNewContext(readProgram,context(nodes)));
  assert.equal(read.controls[0].options.length,2);
  assert.equal(read.controls[0].options[0].selector,'#brown');
  assert.throws(() => act(combo,'select',{option_selector:'#unrelated'},nodes), /exactly one visible ARIA-owned/);
  assert.throws(() => act(combo,'select',{option_selector:'#disabled'},nodes), /disabled/);
  brown.click = () => { brown.attrs['aria-selected'] = 'true'; };
  act(combo,'select',{option_selector:'#brown'},nodes);
  assert.equal(inspect(combo,{text:'Brown'},nodes).matches,true);
  brown.hidden = true;
  assert.equal(inspect(combo,{text:'Brown'},nodes).matches,true);
});
test('autocomplete fill preserves popup focus; observed option activation survives unmounted popup', () => {
  const combo = field(); combo.attrs.role='combobox'; combo.attrs['aria-controls']='popup'; combo.attrs['aria-expanded']='true';
  const popup = new Element('div',{id:'popup',role:'listbox'});
  const choice = popup.append(new Element('div',{id:'choice',role:'option'})); choice.innerText='Self employed';
  let blurred = false;combo.blur=()=>{blurred=true;popup.hidden=true;};
  act(combo,'fill',{text:'Self employed'},[combo,popup,choice]);assert.equal(blurred,false);
  assert.equal(inspect(combo,{text:'Self employed'},[combo,popup,choice]).matches,false);
  choice.click=()=>{combo.value='Self employed';combo.attrs['aria-expanded']='false';};
  const applied=JSON.parse(act(combo,'select',{option_selector:'#choice'},[combo,popup,choice]));
  assert.equal(inspect(combo,applied.verification,[combo]).matches,true);
  assert.equal(inspect(combo,{text:'Self employed'},[combo]).matches,false);
  const displayed=inspect(combo,{text:'Self employed',comparison:'displayed_value'},[combo]);
  assert.equal(displayed.matches,true);assert.equal(displayed.evidence,'displayed_value_only');
});
test('navigation preflight race guard refuses changed/private documents before marking them', () => {
  const script=program('mark-navigation');const ctx=context([]);
  const run=args=>JSON.parse(vm.runInNewContext(`(() => {const args=${JSON.stringify(args)};${script}})()`,ctx));
  for (const current of ['http://localhost/private','https://private.internal/','https://example.com/different']) {
    ctx.location.href=current;
    assert.throws(()=>run({marker:'fresh-marker',url:'https://example.com/form'}),/page changed before navigation/);
    assert.equal(ctx.document.__zeroclawNavigationMarker,undefined);
  }
  ctx.location.href='https://example.com/form';
  assert.throws(()=>run({marker:'fresh-marker',url:''}),/page changed before navigation/);
  assert.equal(ctx.document.__zeroclawNavigationMarker,undefined);
  assert.equal(run({marker:'fresh-marker',url:ctx.location.href}).url,ctx.location.href);
  assert.equal(ctx.document.__zeroclawNavigationMarker,'fresh-marker');
});
test('navigation identity rejects old document and supports only the intended fragment transition', () => {
  const script=program('navigation');const ctx=context([]);ctx.document.__zeroclawNavigationMarker='marker';
  const run=args=>JSON.parse(vm.runInNewContext(`(() => {const args=${JSON.stringify(args)};${script}})()`,ctx));
  assert.equal(run({marker:'marker'}).ready,false);
  assert.equal(run({marker:'marker',fragment_only:true,url:'https://example.com/other'}).ready,false);
  assert.equal(run({marker:'marker',fragment_only:true,url:ctx.location.href}).ready,true);
  delete ctx.document.__zeroclawNavigationMarker;assert.equal(run({marker:'marker'}).ready,true);
});
test('postchecks catch asynchronous framework reset, blur validation, and navigation', async () => {
  const input = field();
  input.dispatchEvent = event => { if (event.type === 'input') setTimeout(() => {input.value = '';},20); return true; };
  act(input,'fill',{text:'Brown'});
  assert.equal(inspect(input,{text:'Brown'}).matches,true);
  await new Promise(resolve => setTimeout(resolve,40));
  assert.equal(inspect(input,{text:'Brown'}).status,'value_mismatch');
  input.dispatchEvent = () => true; input.blur = () => {input.attrs['aria-invalid'] = 'true';};
  act(input,'fill',{text:'Brown'});
  assert.equal(inspect(input,{text:'Brown'}).status,'validation_failed');
  assert.equal(inspect(input,{text:'Brown',url:'https://example.com/old'}).status,'page_changed');
});
test('verification never returns actual values and refuses password comparisons', () => {
  const input = field(); input.value = 'private-fixture-value';
  const result = inspect(input,{text:'other'});
  assert.equal(result.matches,false); assert.equal(JSON.stringify(result).includes(input.value),false);
  const password = field('password'); password.value = 'private-password-fixture';
  assert.throws(() => inspect(password,{text:password.value}),/Password values cannot be compared/);
  const presence = inspect(password,{presence_only:true});
  assert.equal(presence.evidence,'presence_only'); assert.equal(JSON.stringify(presence).includes(password.value),false);
});
test('readiness identifies delayed/hidden/disabled controls, busy states, and ambiguous targets', () => {
  const input = field(); const ctx = context([input]);
  assert.equal(ready(ctx,{selector:'#field'}).ready,true);
  ctx.document.readyState = 'loading'; assert.ok(ready(ctx).reasons.includes('document_loading'));
  ctx.document.readyState = 'complete';
  assert.ok(ready(ctx,{selector:'#missing'}).reasons.includes('expected_control_missing'));
  input.hidden = true; assert.ok(ready(ctx,{selector:'#field'}).reasons.includes('expected_control_hidden'));
  input.hidden = false; input.disabled = true; assert.ok(ready(ctx,{selector:'#field'}).reasons.includes('expected_control_disabled'));
  input.disabled = false; input.attrs['aria-busy'] = 'true'; assert.ok(ready(ctx).reasons.includes('aria_busy'));
  assert.throws(() => ready(context([input,field()]),{selector:'#field'}),/multiple elements/);
});
test('date action refuses custom widgets and malformed dates before mutation, reader exposes constraints', () => {
  const input = field(); assert.throws(() => act(input,'set_date',{text:'2020-01-02'}),/native date input/);
  const date = field('date'); date.attrs.min = '2020-01-01'; date.attrs.max = '2030-01-01';
  assert.throws(() => act(date,'set_date',{text:'01/02/2020'}),/YYYY-MM-DD/);
  assert.equal(date.value,'');
  act(date,'set_date',{text:'2020-01-02'}); assert.equal(inspect(date,{text:'2020-01-02'}).matches,true);
  const control = JSON.parse(vm.runInNewContext(readProgram,context([date]))).controls[0];
  assert.equal(control.dateInputFormat,'YYYY-MM-DD'); assert.equal(control.constraints.min,'2020-01-01');
});
(async () => {
  for (const [name,body] of pending) { await body(); assertions++; process.stdout.write(`ok ${name}\n`); }
  process.stdout.write(`${assertions} offline DOM fixtures passed\n`);
})().catch(error => { console.error(error); process.exitCode = 1; });
