const visible = element => {
  const style = getComputedStyle(element);
  return element.getClientRects().length > 0 && style.display !== 'none' &&
    style.visibility !== 'hidden' && style.visibility !== 'collapse' &&
    !element.closest('[hidden],[aria-hidden="true"]');
};
const disabled = element => element.matches(':disabled') || element.getAttribute('aria-disabled') === 'true';
const controlsSelector = 'a,button,input:not([type="hidden"]),select,textarea,[role=button],[role=checkbox],[role=radio],[role=combobox],[role=listbox],[role=textbox],[contenteditable="true"]';
const selectorFor = element => {
  const unique = selector => {
    const matches = document.querySelectorAll(selector);
    return matches.length === 1 && matches[0] === element;
  };
  if (element.id) {
    const selector = `#${CSS.escape(element.id)}`;
    if (unique(selector)) return selector;
  }
  const parts = [];
  let current = element;
  while (current && current.nodeType === 1) {
    let part = current.tagName.toLowerCase();
    const siblings = current.parentElement ? [...current.parentElement.children].filter(x => x.tagName === current.tagName) : [];
    if (siblings.length > 1) part += `:nth-of-type(${siblings.indexOf(current) + 1})`;
    parts.unshift(part);
    const selector = parts.join(' > ');
    if (unique(selector)) return selector;
    current = current.parentElement;
  }
  throw new Error('Could not create a unique control selector');
};
const target = selector => {
  const matches = document.querySelectorAll(selector);
  if (matches.length === 0) throw new Error('Element not found');
  if (matches.length !== 1) throw new Error('Selector matches multiple elements; read the page for a unique selector');
  const element = matches[0];
  if (!visible(element)) throw new Error('Element is not visible');
  if (disabled(element)) throw new Error('Element is disabled');
  return element;
};
const customSelect = element => ['combobox', 'listbox'].includes(element.getAttribute('role'));
// Only explicit ARIA ownership connects a portalled popup to its control. Never
// guess that an unrelated option elsewhere in the document belongs to it.
const ownedOptions = element => {
  const roots = [element];
  const ids = `${element.getAttribute('aria-controls') || ''} ${element.getAttribute('aria-owns') || ''}`.trim().split(/\s+/).filter(Boolean);
  for (const id of ids) {
    const matches = document.querySelectorAll(`#${CSS.escape(id)}`);
    if (matches.length === 1) roots.push(matches[0]);
  }
  return [...new Set(roots.flatMap(root => [...root.querySelectorAll('[role="option"]')]))];
};
const optionLabel = option => (option.getAttribute('aria-label') || option.innerText || '').trim();
const validityFor = element => {
  const validity = element.validity;
  return {
    valid: (!validity || validity.valid) && element.getAttribute('aria-invalid') !== 'true',
    ariaInvalid: element.getAttribute('aria-invalid') === 'true',
    ...(validity ? {valueMissing: validity.valueMissing, typeMismatch: validity.typeMismatch,
      patternMismatch: validity.patternMismatch, rangeUnderflow: validity.rangeUnderflow,
      rangeOverflow: validity.rangeOverflow, stepMismatch: validity.stepMismatch,
      badInput: validity.badInput, customError: validity.customError} : {})
  };
};
