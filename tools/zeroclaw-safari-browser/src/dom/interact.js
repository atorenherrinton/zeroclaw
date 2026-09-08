const element = target(args.selector);
const notifyValueChange = () => {
  element.dispatchEvent(new Event('input', {bubbles: true, composed: true}));
  element.dispatchEvent(new Event('change', {bubbles: true, composed: true}));
};
let verification = null;
if (args.action === 'click') {
  element.click();
} else if (args.action === 'fill' || args.action === 'set_date') {
  if (element.readOnly) throw new Error('Element is read-only');
  const tag = element.tagName;
  let prototype;
  if (tag === 'INPUT') {
    if (!['text','search','tel','url','email','password','number','date','datetime-local','month','time','week'].includes(element.type)) {
      throw new Error('Fill requires a text input or textarea; use select or check for those controls');
    }
    prototype = HTMLInputElement.prototype;
  } else if (tag === 'TEXTAREA') prototype = HTMLTextAreaElement.prototype;
  else throw new Error('Fill requires a text input or textarea');
  if (args.action === 'set_date') {
    if (tag !== 'INPUT' || element.type !== 'date') throw new Error('set_date requires a native date input; use the observed format or native Safari Computer calendar controls for custom date widgets');
    if (!/^\d{4}-\d{2}-\d{2}$/.test(args.text)) throw new Error('Date must use YYYY-MM-DD');
  }
  // Validate native constraints on a detached input before touching the page.
  // Application-specific rejection is separately observed after input/change.
  if (tag === 'INPUT' && element.type !== 'password') {
    const probe = element.cloneNode(false);
    Object.getOwnPropertyDescriptor(prototype, 'value').set.call(probe, args.text);
    if (probe.value !== args.text || (probe.validity && !probe.validity.valid)) throw new Error('Value violates the input format or constraints; inspect the returned control metadata');
  }
  element.focus();
  Object.getOwnPropertyDescriptor(prototype, 'value').set.call(element, args.text);
  if (element.value !== args.text) throw new Error('The input rejected that value');
  notifyValueChange();
  if (element.value !== args.text) throw new Error('The page did not retain the filled value');
  // Autocomplete popups commonly close on blur. Keep their search field focused
  // so the next read can enumerate options; typed text is still uncommitted.
  if (!customSelect(element)) element.blur();
  // Password comparison stays entirely inside this program and is never exposed
  // as a verification oracle. The later postcheck reports presence only.
  verification = element.type === 'password' ? {password: true} : {text: args.text};
} else if (args.action === 'select') {
  if (element.tagName === 'SELECT') {
    if (element.multiple) throw new Error('Select requires a single-selection select field');
    if (args.option_selector) throw new Error('Native select requires its exact option value in text');
    const options = [...element.options].filter(option => option.value === args.text);
    if (options.length !== 1) throw new Error('Option value must match exactly one option');
    const option = options[0];
    if (option.disabled || option.parentElement?.disabled) throw new Error('Option is disabled');
    element.focus();
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(element, args.text);
    if (!option.selected) throw new Error('The select field rejected that option');
    notifyValueChange();
    if (element.value !== args.text || !option.selected) throw new Error('The page did not retain the selected option');
    element.blur();
    verification = {text: args.text};
  } else if (customSelect(element)) {
    if (element.getAttribute('aria-multiselectable') === 'true') throw new Error('Select requires a single-selection control');
    const owned = ownedOptions(element).filter(visible);
    const candidates = args.option_selector ? [target(args.option_selector)].filter(option => owned.includes(option)) : owned.filter(option => optionLabel(option) === args.text);
    if (candidates.length !== 1) throw new Error('Select requires exactly one visible ARIA-owned option; open the control, read its returned options, and use option_selector');
    const option = candidates[0];
    if (disabled(option)) throw new Error('Option is disabled');
    const label = optionLabel(option);
    option.click();
    verification = {text: label, option_activated: true};
  } else throw new Error('Select requires a single-selection select field or an ARIA combobox/listbox');
} else if (args.action === 'check' || args.action === 'uncheck') {
  if (element.tagName !== 'INPUT' || element.type !== 'checkbox') throw new Error('Check and uncheck require a checkbox input');
  const expected = args.action === 'check';
  if (element.checked !== expected) element.click();
  if (element.checked !== expected) throw new Error('The checkbox did not reach the requested state');
  verification = {checked: expected};
} else if (args.action === 'press') {
  if (args.key !== 'Enter') throw new Error('Only Enter is supported; other keys require native Safari computer controls');
  const tag = element.tagName;
  if (customSelect(element)) throw new Error('Enter on a combobox is not form submission; select an observed option or use native Safari Computer controls');
  if (tag === 'BUTTON' || (tag === 'A' && element.href) ||
      (tag === 'INPUT' && ['submit','button','reset'].includes(element.type))) element.click();
  else throw new Error('Enter only activates links/buttons; explicitly click the intended submit button, or use native Safari Computer controls for text-field keys');
} else throw new Error('Unsupported interaction');
return JSON.stringify({applied: true, verification});
