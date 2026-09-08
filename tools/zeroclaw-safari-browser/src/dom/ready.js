const reasons = [];
if (document.visibilityState === 'hidden') reasons.push('document_hidden');
if (document.readyState !== 'complete') reasons.push('document_loading');
if (!document.body) reasons.push('body_missing');
if (pendingCustomElements().length) reasons.push('custom_elements_pending');
if (deepQuery('[aria-busy="true"]').some(visible)) reasons.push('aria_busy');
if (args.selector) {
  const matches = args.selector.startsWith('shadow:') ? queryPath(args.selector) : deepQuery(args.selector);
  if (matches.length > 1) throw new Error('Expected selector matches multiple elements');
  if (matches.length === 0) reasons.push('expected_control_missing');
  else if (!visible(matches[0])) reasons.push('expected_control_hidden');
  else if (disabled(matches[0])) reasons.push('expected_control_disabled');
}
const controls = deepQuery(controlsSelector).filter(visible).slice(0, 160);
// A transient fingerprint is used only for this bounded wait. No form values
// or durable DOM snapshots are stored in the connector.
const fingerprint = JSON.stringify([location.href, document.title, composedText(document.body).length,
  controls.map(element => [element.tagName, element.id, element.getAttribute('role'), disabled(element),
    Boolean(element.value), element.getAttribute('aria-expanded'), element.getAttribute('aria-invalid')])]);
return JSON.stringify({ready: reasons.length === 0, reasons, fingerprint, url: location.href});
