#!/usr/bin/env bash
# Sintassi JavaScript della console: ogni <script> inline di app.html, superadmin.html e
# worker/index.html, più app.js, deve essere JavaScript valido. Un apostrofo non escapato in una
# stringa (25/11/2026, commit 77a5d87) bastava a non far partire l'intera console.
# Non richiede il core: solo node.
set -uo pipefail
cd "$(dirname "$0")/.."
command -v node >/dev/null || { echo "SKIP: node non disponibile"; exit 0; }
node - <<'JS'
const fs = require('fs'), vm = require('vm');
let bad = 0, n = 0;
for (const f of ['console/static/app.html', 'console/static/superadmin.html', 'console/static/worker/index.html']) {
  const h = fs.readFileSync(f, 'utf8'); const re = /<script>([\s\S]*?)<\/script>/g; let m, i = 0;
  while ((m = re.exec(h))) { i++; n++;
    try { new vm.Script(m[1], { filename: `${f}#script${i}` }); }
    catch (e) { bad++; console.log(`FAIL  ${f} script ${i}: ${String(e.stack).split('\n').slice(0, 2).join(' | ').slice(0, 300)}`); } }
}
try { new vm.Script(fs.readFileSync('console/static/app.js', 'utf8'), { filename: 'app.js' }); n++; }
catch (e) { bad++; console.log('FAIL  app.js: ' + e.message); }
console.log(`console-syntax: ${n - bad} passed, ${bad} failed`);
process.exit(bad ? 1 : 0);
JS
