if (document.visibilityState === 'hidden') return JSON.stringify({matches:false,status:'document_hidden'});
if (args.url && location.href !== args.url) return JSON.stringify({matches: false, status: 'page_changed'});
const matches = queryPath(args.selector);
if (matches.length !== 1) return JSON.stringify({matches: false, status: matches.length ? 'ambiguous' : 'control_missing'});
const element = matches[0];
if (element.type === 'password') {
  if (args.presence_only) return JSON.stringify({matches: Boolean(element.value), status: 'presence_only', evidence: 'presence_only'});
  throw new Error('Password values cannot be compared or returned; use AutoFill presence flags');
}
if (!visible(element)) return JSON.stringify({matches: false, status: 'control_hidden'});
const validity = validityFor(element);
let equal = false;
let evidence = 'value';
if (typeof args.checked === 'boolean') {
  if (element.tagName !== 'INPUT' || !['checkbox', 'radio'].includes(element.type)) throw new Error('Checked verification requires a checkbox or radio input');
  equal = element.checked === args.checked;
  evidence = 'checked';
} else if (customSelect(element)) {
  const options = ownedOptions(element);
  const selected = options.filter(option => option.getAttribute('aria-selected') === 'true');
  // An editable combobox's text alone may be an uncommitted search query.
  // Require selected-option state, even if the popup has since been hidden.
  equal = selected.length === 1 && optionLabel(selected[0]) === args.text;
  evidence = 'aria_selected_option';
  const display = element.tagName === 'INPUT' ? element.value :
    (element.getAttribute('aria-valuetext') || composedText(element, 1024) || '').trim();
  if (args.comparison === 'displayed_value') {
    equal = display === args.text;
    evidence = 'displayed_value_only';
  } else if (!equal && args.option_activated && options.length === 0 && element.getAttribute('aria-expanded') === 'false') {
    // Some widgets unmount the popup on selection. Only the internal select
    // action can supply observed option-activation evidence. Fill cannot.
    equal = display === args.text;
    evidence = 'observed_option_activation_and_display_value';
  }
} else if (['INPUT', 'TEXTAREA', 'SELECT'].includes(element.tagName)) {
  equal = element.value === args.text;
} else throw new Error('Verification requires a form input, select, or ARIA selection control');
return JSON.stringify({matches: equal && validity.valid, status: !validity.valid ? 'validation_failed' : equal ? 'matched' : 'value_mismatch', validity, evidence});
