// Resolve controls from the current DOM on every call. Paths name each open
// shadow host explicitly; IDs are only unique within their own tree scope.
const rootOf = element => element.getRootNode ? element.getRootNode() : document;
const parentOf = element => element.assignedSlot || element.parentElement || rootOf(element).host || null;
let discoveredRoots;
const openRoots = () => {
  if (discoveredRoots) return discoveredRoots;
  const roots = [document];
  let visited = 0;
  for (let i = 0; i < roots.length; i++) {
    for (const element of roots[i].querySelectorAll('*')) {
      if (++visited > 50000) throw new Error('DOM inspection limit exceeded; use Computer browser controls');
      if (element.shadowRoot) {
        if (roots.length >= 256) throw new Error('Shadow root inspection limit exceeded; use Computer browser controls');
        roots.push(element.shadowRoot);
      }
    }
  }
  discoveredRoots = roots;
  return roots;
};
const deepQuery = selector => openRoots().flatMap(root => [...root.querySelectorAll(selector)]);
const queryPath = selector => {
  if (!selector.startsWith('shadow:')) return [...document.querySelectorAll(selector)];
  let parts;
  try { parts = JSON.parse(selector.slice(7)); } catch { throw new Error('Invalid shadow control path'); }
  if (!Array.isArray(parts) || parts.length < 2 || parts.length > 16 || parts.some(p => typeof p !== 'string' || !p)) throw new Error('Invalid shadow control path');
  let root = document;
  for (let i = 0; i < parts.length - 1; i++) {
    const hosts = root.querySelectorAll(parts[i]);
    if (hosts.length !== 1 || !hosts[0].shadowRoot) throw new Error('Shadow host is missing or ambiguous; read the page again');
    root = hosts[0].shadowRoot;
  }
  return [...root.querySelectorAll(parts[parts.length - 1])];
};
// Slots contribute their assigned nodes, not their fallback text. Input values,
// including passwords, are never part of this text projection.
const composedText = (element, limit = 50000) => {
  let text = '', visited = 0;
  const seen = new Set();
  const walk = node => {
    if (!node || seen.has(node) || text.length >= limit) return;
    if (++visited > 50000) throw new Error('DOM text inspection limit exceeded; use Computer browser controls');
    seen.add(node);
    if (node.nodeType === 3) { text += (node.textContent || '').slice(0, limit - text.length); return; }
    if (node.nodeType === 1) {
      if (['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','INPUT','TEXTAREA','SELECT'].includes(node.tagName)) return;
      const style = getComputedStyle(node);
      if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse' || node.getAttribute('aria-hidden') === 'true' || node.hasAttribute?.('hidden')) return;
    }
    const assigned = node.tagName === 'SLOT' && node.assignedNodes ? node.assignedNodes({flatten:true}) : [];
    const children = assigned.length ? assigned : node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes;
    if (children) for (const child of children) walk(child);
    if (text.length < limit && ['DIV','P','BR','LI','BUTTON','A'].includes(node.tagName)) text += '\n';
  };
  walk(element);
  return text.trim();
};
const hiddenInComposedTree = element => {
  for (let node = element; node; node = parentOf(node)) {
    if (node.getAttribute('hidden') !== null || node.getAttribute('aria-hidden') === 'true' || node.getAttribute('inert') !== null) return true;
    const style = getComputedStyle(node);
    if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return true;
  }
  return false;
};
const visible = element => {
  const style = getComputedStyle(element);
  return element.getClientRects().length > 0 && style.display !== 'none' &&
    style.visibility !== 'hidden' && style.visibility !== 'collapse' &&
    !hiddenInComposedTree(element);
};
const disabled = element => {
  for (let node = element; node; node = parentOf(node)) {
    if (node.matches(':disabled') || node.getAttribute('aria-disabled') === 'true' || (node.tagName.includes('-') && node.hasAttribute?.('disabled'))) return true;
  }
  return false;
};
const controlsSelector = 'a,button,input:not([type="hidden"]),select,textarea,[role=button],[role=checkbox],[role=radio],[role=combobox],[role=listbox],[role=textbox],[contenteditable="true"]';
const localSelectorFor = (element, root) => {
  const unique = selector => {
    const matches = root.querySelectorAll(selector);
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
    const siblings = [...((current.parentElement || root).children || [])].filter(x => x.tagName === current.tagName);
    if (siblings.length > 1) part += `:nth-of-type(${siblings.indexOf(current) + 1})`;
    parts.unshift(part);
    const selector = parts.join(' > ');
    if (unique(selector)) return selector;
    current = current.parentElement;
  }
  throw new Error('Could not create a unique control selector');
};
const selectorFor = element => {
  const parts = [];
  let node = element;
  for (let depth = 0; node; depth++) {
    if (depth >= 16) throw new Error('Shadow control path is too deep; use Computer browser controls');
    const root = rootOf(node);
    parts.unshift(localSelectorFor(node, root));
    node = root.host || null;
  }
  const selector = parts.length === 1 ? parts[0] : `shadow:${JSON.stringify(parts)}`;
  if (new TextEncoder().encode(selector).length > 1024) throw new Error('Control path is too long; use Computer browser controls');
  return selector;
};
const target = selector => {
  if (document.visibilityState === 'hidden') throw new Error('Safari page is hidden; unlock the Mac and show the dedicated Safari window before interacting');
  const matches = queryPath(selector);
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
    const matches = rootOf(element).querySelectorAll(`#${CSS.escape(id)}`);
    if (matches.length === 1) roots.push(matches[0]);
  }
  return [...new Set(roots.flatMap(root => [...root.querySelectorAll('[role="option"]')]))];
};
const optionLabel = option => (option.getAttribute('aria-label') || composedText(option, 1024) || '').trim();
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

const pendingCustomElements = () => typeof customElements === 'undefined' ? [] : deepQuery('*').filter(element => element.tagName.includes('-') && !element.shadowRoot && !customElements.get(element.tagName.toLowerCase()));

const controlText = element => {
  const label = element.labels && element.labels.length ? composedText(element.labels[0], 1024) : '';
  const own = element.getAttribute('aria-label') || composedText(element, 1024) || label || element.getAttribute('placeholder') || element.getAttribute('title') || '';
  if (own.trim()) return own.trim();
  const root = rootOf(element);
  if (root.host && root.querySelectorAll(controlsSelector).length === 1) return root.host.getAttribute('aria-label') || '';
  return '';
};
