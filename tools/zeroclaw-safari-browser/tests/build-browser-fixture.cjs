// Generate a self-contained local Safari test page embedding the exact fixed
// production DOM programs. This does not add a local-URL exception to the MCP.
const fs = require('node:fs');
const path = require('node:path');
const destination = process.argv[2];
if (!destination) throw new Error('Usage: node build-browser-fixture.cjs /absolute/path/fixture.html');
const programs = Object.fromEntries(['common','read','interact','inspect','ready','navigation'].map(name =>
  [name, fs.readFileSync(path.join(__dirname, `../src/dom/${name}.js`),'utf8')]));
const template = fs.readFileSync(path.join(__dirname,'browser-fixture.html'),'utf8');
const encoded = JSON.stringify(programs).replaceAll('<','\\u003c');
fs.writeFileSync(destination, template.replace('__DOM_PROGRAMS__',encoded));
process.stdout.write(`${destination}\n`);
