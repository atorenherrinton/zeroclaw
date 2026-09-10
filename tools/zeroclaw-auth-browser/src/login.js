// Fixed, synchronous login fill. Credential arguments are never returned,
// interpolated into script source, persisted in page globals, or submitted.
if (window !== window.top || location.protocol !== 'https:' || location.origin !== args.origin) {
  throw new Error('Login origin changed');
}
if (!['both', 'username', 'password'].includes(args.field)) throw new Error('Unsupported login field');

const usableInput = (element, role) => element instanceof HTMLInputElement &&
  element.ownerDocument === document && element.isConnected && visible(element) &&
  !disabled(element) && !element.readOnly &&
  (role === 'password' ? element.type === 'password' : ['text', 'email'].includes(element.type));
const sameOriginForm = element => {
  const form = element.form;
  if (form) {
    if (!form.isConnected || form.ownerDocument !== document) throw new Error('Login form changed');
    const action = new URL(form.action || location.href, location.href);
    if (action.protocol !== 'https:' || action.origin !== args.origin || action.username || action.password) {
      throw new Error('Login form has another origin');
    }
  }
  return {form, action: form ? form.action : null};
};
const pick = (role, selector, scopeForm) => {
  if (selector !== null && selector !== undefined) {
    // A caller-supplied selector must resolve uniquely before filtering, so it
    // cannot accidentally match both a legitimate field and a hidden decoy.
    const found = queryPath(selector);
    if (found.length !== 1 || !usableInput(found[0], role)) throw new Error('Unsafe login selector');
    return found[0];
  }
  let found = deepQuery('input').filter(element => usableInput(element, role));
  if (scopeForm) found = found.filter(element => element.form === scopeForm);
  if (role === 'username') {
    // Semantic hints may narrow a multi-field form; never pick an arbitrary
    // first field when a tier has multiple plausible username inputs.
    const tiers = [
      element => element.autocomplete.toLowerCase().split(/\s+/).includes('username'),
      element => element.type === 'email' || element.autocomplete.toLowerCase().split(/\s+/).includes('email'),
      element => /^(?:user(?:name)?|email|login|identifier|user[_-]?(?:name|id)|email[_-]?address)$/i.test(element.name || '') ||
        /^(?:user(?:name)?|email|login|identifier|user[_-]?(?:name|id)|email[_-]?address)$/i.test(element.id || '')
    ];
    for (const tier of tiers) {
      const matches = found.filter(tier);
      if (matches.length) { found = matches; break; }
    }
  }
  if (found.length !== 1) throw new Error('Login field is missing or ambiguous');
  return found[0];
};

const passwordField = args.field === 'username' ? null : pick('password', args.password_selector, null);
const usernameField = args.field === 'password' ? null : pick('username', args.username_selector, passwordField?.form || null);
if (usernameField && passwordField && usernameField.form !== passwordField.form) {
  throw new Error('Login fields belong to different forms');
}
const targets = [
  ...(usernameField ? [{element:usernameField, role:'username', selector:args.username_selector, value:args.username}] : []),
  ...(passwordField ? [{element:passwordField, role:'password', selector:args.password_selector, value:args.password}] : [])
].map(item => ({...item, ...sameOriginForm(item.element)}));
const assertUnchanged = () => {
  if (window !== window.top || location.protocol !== 'https:' || location.origin !== args.origin) {
    throw new Error('Login origin changed');
  }
  for (const item of targets) {
    if (!usableInput(item.element, item.role)) throw new Error('Login field changed');
    const current = sameOriginForm(item.element);
    if (current.form !== item.form || current.action !== item.action) throw new Error('Login form changed');
    if (pick(item.role, item.selector, item.role === 'username' ? passwordField?.form || null : null) !== item.element) {
      throw new Error('Login field selection changed');
    }
  }
};
const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set;
if (typeof setter !== 'function') throw new Error('Native input setter unavailable');
const filled = {username_filled:false, password_filled:false};
assertUnchanged();
for (const item of targets) {
  // Input/change handlers can replace fields or rewrite form actions. Recheck
  // every target immediately before each value injection and after each event.
  assertUnchanged();
  setter.call(item.element, item.value);
  filled[`${item.role}_filled`] = true;
  item.element.dispatchEvent(new Event('input', {bubbles:true, composed:true}));
  assertUnchanged();
  item.element.dispatchEvent(new Event('change', {bubbles:true, composed:true}));
  assertUnchanged();
}
return filled;
