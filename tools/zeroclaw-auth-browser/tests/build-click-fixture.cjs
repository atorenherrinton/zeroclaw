const fs = require('node:fs');
const path = require('node:path');
const common = fs.readFileSync(path.join(__dirname,'../src/dom/common.js'),'utf8');
const click = fs.readFileSync(path.join(__dirname,'../src/click-point.js'),'utf8');
const body = `<!doctype html><meta charset="utf-8"><title>Click target fixture</title>
<style>button{width:160px;height:60px} #overlay{position:fixed;inset:0;background:white;z-index:9999;display:none}</style>
<h1 id="status">Running</h1><x-host id="host"></x-host><div id="overlay"></div>
<script>
const host=document.querySelector('#host');
host.attachShadow({mode:'open'}).innerHTML='<style>button{width:160px;height:60px}</style><button id="target"><span>Choose date</span></button>';
const run = new Function('args', ${JSON.stringify(common + '\n' + click)});
const args={selector:'shadow:["#host","#target"]'};let count=0;
function pass(ok){if(!ok)throw new Error('Assertion failed'); count++}
const point=run(args);pass(Number.isInteger(point.x)&&Number.isInteger(point.y));
document.querySelector('#overlay').style.display='block';
try{run(args);throw new Error('Overlay accepted')}catch(e){pass(e.message.includes('obscured'))}
document.querySelector('#overlay').style.display='none';host.setAttribute('inert','');
try{run(args);throw new Error('Inert accepted')}catch(e){pass(e.message.includes('not visible'))}
host.removeAttribute('inert');host.shadowRoot.querySelector('button').disabled=true;
try{run(args);throw new Error('Disabled accepted')}catch(e){pass(e.message.includes('disabled'))}
host.shadowRoot.querySelector('button').disabled=false;
const semantic=document.createElement('month-view');semantic.setAttribute('role','grid');document.body.append(semantic);
const pending = new Function(${JSON.stringify(common + '\nreturn pendingCustomElements().map(e=>e.tagName);')});
pass(!pending().includes('MONTH-VIEW'));

const old=host.shadowRoot.querySelector('button');old.remove();
try{run(args);throw new Error('Stale accepted')}catch(e){pass(e.message.includes('not found'))}
document.querySelector('#status').textContent='PASS '+count+' click boundary checks';document.title='PASS click boundary';
</script>`;
fs.writeFileSync(process.argv[2],body);
