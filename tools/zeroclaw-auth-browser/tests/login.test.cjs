// Synthetic credentials and DOM fixtures only; no real browser/vault access.
const assert = require('node:assert/strict');
const {test} = require('node:test');
const {readFileSync} = require('node:fs');
const script = readFileSync(new URL('../src/login.js', `file://${__filename}`), 'utf8');
const fill = new Function('args', 'window', 'location', 'document', 'HTMLInputElement', 'visible', 'disabled', 'queryPath', 'deepQuery', script);

function fixture(options = {}) {
  const document = {};
  const location = {origin:'https://example.com', protocol:'https:', href:'https://example.com/login'};
  const window = {}; window.top = window;
  class Input {
    constructor(type, id, form) {
      Object.assign(this, {type,id,name:id,form,autocomplete:'',ownerDocument:document,isConnected:true,shown:true,readOnly:false,disabled:false,events:[]});
      this.storedValue = '';
    }
    get value() { return this.storedValue; }
    set value(value) { this.storedValue = value; }
    dispatchEvent(event) { this.events.push(event.type); this.onEvent?.(event); return true; }
  }
  const form = {action:'https://example.com/session', isConnected:true, ownerDocument:document};
  const username = new Input('email', 'username', form);
  const password = new Input('password', 'password', form);
  const fields = options.fields === 'username' ? [username] : options.fields === 'password' ? [password] : [username,password];
  const selectors = {'#username':[username], '#password':[password]};
  const args = {origin:location.origin, username:'synthetic-user@example.com', password:'synthetic-password-42', field:'both', username_selector:null, password_selector:null};
  const run = () => fill(args, window, location, document, Input, e=>e.shown, e=>e.disabled, s=>selectors[s] || [], ()=>fields);
  return {document, location, window, Input, form, username, password, fields, selectors, args, run};
}

test('ordinary same-origin login fills both fields and returns only booleans', () => {
  const f=fixture();
  assert.deepEqual(f.run(), {username_filled:true,password_filled:true});
  assert.equal(f.username.value, f.args.username);
  assert.equal(f.password.value, f.args.password);
  assert.deepEqual(f.password.events, ['input','change']);
});
test('username-first and password-only forms work independently', () => {
  for(const role of ['username','password']) {
    const f=fixture({fields:role}); f.args.field=role;
    assert.deepEqual(f.run(), {username_filled:role==='username',password_filled:role==='password'});
  }
});
test('formless SPA inputs work on the exact HTTPS origin', () => {
  const f=fixture(); f.username.form=null; f.password.form=null;
  assert.equal(f.run().password_filled,true);
});
test('other origins, insecure pages, and frames never receive credentials', () => {
  for(const mutate of [f=>f.location.origin='https://other.example.com', f=>f.location.protocol='http:', f=>f.window.top={}]) {
    const f=fixture(); mutate(f); assert.throws(f.run); assert.equal(f.username.value,''); assert.equal(f.password.value,'');
  }
});
test('cross-origin form actions and fields in different forms are rejected', () => {
  for(const mutate of [f=>f.form.action='https://other.example.com/session', f=>f.form.action='http://example.com/session', f=>f.password.form={...f.form}]) {
    const f=fixture(); mutate(f); assert.throws(f.run); assert.equal(f.password.value,''); assert.equal(f.username.value,'');
  }
});
test('password fill requires a visible unique writable password INPUT', () => {
  for(const mutate of [f=>f.password.type='text', f=>f.password.shown=false, f=>f.password.disabled=true, f=>f.password.readOnly=true, f=>f.password.isConnected=false, f=>f.password.ownerDocument={}]) {
    const f=fixture(); mutate(f); assert.throws(f.run); assert.equal(f.password.value,'');
  }
});
test('an explicit selector matching a hidden decoy is still ambiguous', () => {
  const f=fixture(); f.args.password_selector='#password';
  const decoy=new f.Input('password','decoy',f.form); decoy.shown=false;
  f.selectors['#password'].push(decoy);
  assert.throws(f.run); assert.equal(f.password.value,'');
});
test('ambiguous automatic fields fail closed, explicit unique selector resolves them', () => {
  const f=fixture(); f.fields.push(new f.Input('password','password-confirm',f.form));
  assert.throws(f.run); assert.equal(f.username.value,'');
  f.args.password_selector='#password'; assert.equal(f.run().password_filled,true);
});
test('username semantic hints select a unique username but reject duplicate hints', () => {
  const f=fixture(); const other=new f.Input('text','search-term',f.form); f.fields.push(other);
  assert.equal(f.run().username_filled,true); assert.equal(other.value,'');
  const g=fixture(); g.fields.push(new g.Input('email','other-email',g.form));
  assert.throws(g.run); assert.equal(g.username.value,'');
});
test('input handlers cannot redirect a later password fill to another form/origin', () => {
  for(const mutate of [f=>f.form.action='https://other.example.com/session', f=>f.password.type='text', f=>f.password.isConnected=false, f=>f.location.origin='https://other.example.com']) {
    const f=fixture(); f.username.onEvent=()=>mutate(f);
    assert.throws(f.run); assert.equal(f.password.value,'');
  }
});
test('field re-resolution prevents a replacement or ambiguous target during events', () => {
  const f=fixture(); f.args.password_selector='#password';
  f.username.onEvent=()=>f.selectors['#password']=[new f.Input('password','replacement',f.form)];
  assert.throws(f.run); assert.equal(f.password.value,'');
});

const browserSource = readFileSync(new URL('../src/browser.rs', `file://${__filename}`), 'utf8');
const rustScript = name => browserSource.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))[1];
const fillField = new Function('args','target','HTMLInputElement','HTMLTextAreaElement',rustScript('FILL_SCRIPT'));
test('ordinary typing has no WebDriver file upload route and rejects a file input', () => {
  const f=fixture(); f.password.type='file';
  assert.throws(()=>fillField({selector:'#password',text:'/tmp/synthetic-fixture'},()=>f.password,f.Input,class {}));
  assert.equal(f.password.value,'');
  assert.ok(!browserSource.includes('/element/{id}/value'));
});
test('ordinary typing uses the native value setter for editable fields', () => {
  const f=fixture();
  assert.equal(fillField({selector:'#username',text:'Synthetic field text'},()=>f.username,f.Input,class {}),true);
  assert.equal(f.username.value,'Synthetic field text');
});
test('keyboard focus refuses file inputs', () => {
  const f=fixture(); f.password.type='file';
  const focus = new Function('args','target','HTMLInputElement',rustScript('FOCUS_SCRIPT'));
  assert.throws(()=>focus({selector:'#password'},()=>f.password,f.Input));
});
test('submission guard rejects a later cross-origin form action and submitter override', () => {
  const guard = new Function('deepQuery','location',rustScript('SUBMIT_GUARD'));
  for (const override of [false,true]) {
    const f=fixture(); f.password.value='synthetic-password';
    const submitter={form:f.form,formAction:'https://other.example.com/login'};
    if (!override) f.form.action='https://other.example.com/login';
    assert.throws(()=>guard(selector=>selector.startsWith('input[')?[f.password]:override?[submitter]:[],f.location));
  }
});
