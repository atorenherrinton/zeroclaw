const element = target(args.selector);
element.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
const r = element.getBoundingClientRect();
const x = Math.floor((Math.max(0,r.left)+Math.min(innerWidth,r.right))/2);
const y = Math.floor((Math.max(0,r.top)+Math.min(innerHeight,r.bottom))/2);
if (x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) throw new Error('Control is outside the viewport');
let hit = document.elementFromPoint(x,y);
for (let i=0; hit?.shadowRoot && i<16; i++) {
 const next = hit.shadowRoot.elementFromPoint(x,y);
 if (!next || next === hit) break;
 hit = next;
}
for (let node=hit; node; node=parentOf(node)) {
 if (node === element) return {x,y};
}
// Slotted text may retarget the hit to its host in Chromium. Accept that host
// only when it owns exactly this one visible actionable control.
const root = rootOf(element);
if (hit === root.host) {
 const owned = [...root.querySelectorAll(controlsSelector)].filter(visible);
 if (owned.length === 1 && owned[0] === element) return {x,y};
}
throw new Error(`Control is obscured by ${hit?.tagName || 'no element'}; read the page before interacting`);
