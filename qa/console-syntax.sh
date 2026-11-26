#!/usr/bin/env bash
# Sintassi JavaScript della console: ogni <script> inline di app.html, superadmin.html e
# worker/index.html e invite.html, più app.js e il traduttore i18n.js / i18n-en.js, deve essere JavaScript valido, senza funzioni definite due volte né const/let di pagina ripetute. Un apostrofo non escapato in una
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
  // due funzioni con lo stesso nome: l'ultima vince in silenzio (FE-4: emptyRow, tenantFilter)
  const seen = {}; for (const mm of h.matchAll(/^(?:async )?function ([A-Za-z0-9_$]+)\(/gm)) seen[mm[1]] = (seen[mm[1]] || 0) + 1;
  for (const [k, v] of Object.entries(seen)) if (v > 1) { bad++; console.log(`FAIL  ${f}: funzione ${k} definita ${v} volte`); }
  while ((m = re.exec(h))) { i++; n++;
    try { new vm.Script(m[1], { filename: `${f}#script${i}` }); }
    catch (e) { bad++; console.log(`FAIL  ${f} script ${i}: ${String(e.stack).split('\n').slice(0, 2).join(' | ').slice(0, 300)}`); } }
}
// const/let di primo livello dichiarate due volte (anche tra app.js e la pagina) bloccano l'intero
// script con «Identifier has already been declared» (25/11/2026: fmtMin in app.js e in app.html)
const top = src => [...src.matchAll(/^(?:const|let)\s+([A-Za-z0-9_$]+)\s*=/gm)].map(x => x[1]);
const appjs = top(fs.readFileSync('console/static/app.js', 'utf8'));
for (const f of ['console/static/app.html', 'console/static/superadmin.html', 'console/static/worker/index.html']) {
  const names = [...appjs, ...top(fs.readFileSync(f, 'utf8'))]; const c = {};
  for (const k of names) c[k] = (c[k] || 0) + 1;
  for (const [k, v] of Object.entries(c)) if (v > 1) { bad++; console.log(`FAIL  ${f}: «${k}» dichiarata ${v} volte a livello di pagina (anche con app.js)`); }
}
for (const f of ['app.js', 'i18n.js', 'i18n-en.js']) {
  try { new vm.Script(fs.readFileSync('console/static/' + f, 'utf8'), { filename: f }); n++; }
  catch (e) { bad++; console.log(`FAIL  ${f}: ` + e.message); } }
// pagina di invito: script inline
{ const h = fs.readFileSync('console/static/invite.html', 'utf8'); let m, i = 0; const re = /<script>([\s\S]*?)<\/script>/g;
  while ((m = re.exec(h))) { i++; n++; try { new vm.Script(m[1], { filename: `invite.html#script${i}` }); }
    catch (e) { bad++; console.log(`FAIL  invite.html script ${i}: ` + e.message); } } }
// dizionario inglese: generato da console/tools/i18n/build.py, i segnaposto {n} uguali nelle due lingue
{ const ctx = { window: {} }; vm.runInNewContext(fs.readFileSync('console/static/i18n-en.js', 'utf8'), ctx); n++;
  const d = ctx.window.YW_I18N_EN || {}; const ph = Object.keys(d.phrases || {}).length;
  const badT = (d.templates || []).filter(([it, en]) => (it.match(/\{\d+\}/g) || []).sort().join() !== (en.match(/\{\d+\}/g) || []).sort().join());
  if (ph < 1000 || badT.length) { bad++; console.log(`FAIL  i18n-en.js: ${ph} frasi, ${badT.length} modelli con segnaposto diversi`); } }
console.log(`console-syntax: ${n - bad} passed, ${bad} failed`);
process.exit(bad ? 1 : 0);
JS
