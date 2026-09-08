const inViewport = element => {
  const rect = element.getBoundingClientRect();
  return rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth;
};
let remainingOptionBytes = 16 * 1024;
const optionsFor = element => {
  const native = element.tagName === 'SELECT';
  const source = native ? [...element.options] : ownedOptions(element).filter(visible);
  const options = [];
  for (const option of source.slice(0, 250)) {
    const label = native ? option.label || '' : optionLabel(option);
    const entry = {
      ...(native ? (option.value.length <= 1024 ? {value: option.value} : {valueOmitted: true}) : {selector: selectorFor(option)}),
      label: label.slice(0, 240),
      ...(label.length > 240 ? {labelTruncated: true} : {}),
      selected: native ? option.selected : option.getAttribute('aria-selected') === 'true',
      disabled: native ? option.disabled || Boolean(option.parentElement?.disabled) : disabled(option)
    };
    const bytes = JSON.stringify(entry).length * 3;
    if (bytes > remainingOptionBytes) break;
    remainingOptionBytes -= bytes;
    options.push(entry);
  }
  return {options, totalOptions: source.length, optionsTruncated: options.length < source.length,
    ...(!native ? {optionsScope: 'visible_aria_owned', expanded: element.getAttribute('aria-expanded')} : {})};
};
const allControls = deepQuery(controlsSelector).filter(visible);
allControls.sort((a, b) => Number(inViewport(b)) - Number(inViewport(a)));
const controls = allControls.slice(0, 160).map(element => {
  const tag = element.tagName.toLowerCase();
  const type = (element.getAttribute('type') || '').toLowerCase();
  const isValueControl = tag === 'input' || tag === 'select' || tag === 'textarea';
  const constraints = {};
  for (const name of ['placeholder', 'inputmode', 'pattern', 'min', 'max', 'step', 'maxlength', 'autocomplete', 'aria-describedby']) {
    const value = element.getAttribute(name);
    if (value !== null) constraints[name] = value.slice(0, 240);
  }
  return {
    selector: selectorFor(element), tag, type, role: element.getAttribute('role') || '',
    name: element.getAttribute('name') || '',
    text: controlText(element).slice(0, 240),
    href: element.href || '', hasValue: isValueControl ? Boolean(element.value) : false,
    checked: (type === 'checkbox' || type === 'radio') ? Boolean(element.checked) : false,
    sensitive: type === 'password', disabled: disabled(element), readOnly: Boolean(element.readOnly),
    required: Boolean(element.required) || element.getAttribute('aria-required') === 'true',
    validity: validityFor(element), constraints, inViewport: inViewport(element),
    ...(type === 'date' ? {dateInputFormat: 'YYYY-MM-DD', dateEntry: 'set_date'} : {}),
    ...(tag === 'select' || customSelect(element) ? optionsFor(element) : {})
  };
});
return JSON.stringify({
  url: location.href, title: document.title, readyState: document.readyState,
  text: composedText(document.body), controls,
  rendering: {openShadowRoots: openRoots().length - 1, pendingCustomElements: pendingCustomElements().length, visibility: document.visibilityState},
  totalControls: allControls.length, controlsTruncated: allControls.length > controls.length,
  viewport: {width: innerWidth, height: innerHeight, scrollX, scrollY}
});
