/* YouWorkforce — traduttore dell'interfaccia IT -> EN (console, superadmin, app del lavoratore, invito).
 * L'italiano è la lingua sorgente scritta nelle pagine; con lingua EN ogni testo mostrato viene
 * cercato nel dizionario /i18n-en.js (frasi esatte + modelli con parti variabili {0},{1}…).
 * I dati (nomi, numeri, valori) non sono nel dizionario e restano come sono.
 * Lingua: localStorage 'yw_lang' ('it'|'en'), altrimenti quella del browser. Rigenerare il
 * dizionario dopo aver aggiunto testi: console/tools/i18n/README.md. */
(function(){
  let lang = 'it';
  try { lang = localStorage.getItem('yw_lang') || (navigator.language || 'it').slice(0, 2); } catch (e) {}
  if (lang !== 'en') lang = 'it';
  const D = { ph: Object.create(null), tpl: [] };
  function esc(s){ return s.replace(/[.*+?^$()|[\]\\]/g, '\\$&'); }
  function load(){
    const src = window.YW_I18N_EN; if (!src) return;
    for (const k in (src.phrases || {})) D.ph[k] = src.phrases[k];
    D.tpl = (src.templates || []).map(([it, en]) => {
      // {n} -> gruppo non vuoto; spazi flessibili
      const parts = it.split(/(\{\d+\})/);
      const re = '^' + parts.map(p => /^\{\d+\}$/.test(p) ? '(.+?)' : esc(p).replace(/ /g, '\\s+')).join('') + '$';
      const order = (it.match(/\{\d+\}/g) || []).map(x => +x.slice(1, -1));
      try { return { re: new RegExp(re, 's'), en, order }; } catch (e) { return null; }
    }).filter(Boolean);
  }
  load();
  /** Aggiunge frasi (non sovrascrive quelle del dizionario generato). */
  function extend(map){ for (const k in map) if (D.ph[k] == null) D.ph[k] = map[k]; }
  function trText(s){
    if (s == null) return s; const k = s.replace(/\s+/g, ' ').trim(); if (!k || k.length > 600) return s;
    let v = D.ph[k];
    if (v == null) { // "Etichetta (12)" / "Etichetta: 12"
      const m = k.match(/^(.+?)\s*(\(\d+\)|:\s*.+)$/);
      if (m && D.ph[m[1]] != null) v = D.ph[m[1]] + (m[2].startsWith('(') ? ' ' : '') + m[2];
    }
    if (v == null) for (const t of D.tpl) { const m = k.match(t.re); if (m) {
      const vals = {}; t.order.forEach((n, i) => { vals[n] = m[i + 1]; });
      v = t.en.replace(/\{(\d+)\}/g, (_, n) => { const x = vals[n]; return x == null ? '' : (D.ph[x.trim()] ?? x); }); break; } }
    if (v == null) { // «12 allarmi», «— Testo», «⚠ Testo», «🔧 Testo»: prefisso numerico o simbolo + frase nota
      const m = k.match(/^([\d.,]+\s+|[—–•·▢⚠✓✗→↳🔎📞📟🚨🔧👥:]️?\s*)(.+)$/u);
      if (m) { const r = trText(m[2]); if (r !== m[2]) v = m[1] + r; }
    }
    if (v == null) { // «Apri →», «Carico…», «Nome:»: frase nota + simbolo finale
      const m = k.match(/^(.+?)(\s*[→↗›»…:.!?]+)$/u);
      if (m) { const r = trText(m[1]); if (r !== m[1]) v = r + m[2]; }
    }
    if (v == null && k.includes(' · ')) { // «Sicurezza · Vedere allarmi»: ogni parte a sé
      const parts = k.split(' · '); const tr = parts.map(p => trText(p));
      if (tr.some((p, i) => p !== parts[i])) v = tr.join(' · ');
    }
    if (v == null) { // «Etichetta: testo» con entrambe le parti note
      const m = k.match(/^([^:]{2,80}):\s+(.+)$/);
      if (m) { const a = trText(m[1]), b = trText(m[2]); if (a !== m[1] || b !== m[2]) v = a + ': ' + b; }
    }
    if (v == null || v === k) return s;
    const lead = s.match(/^\s*/)[0], trail = s.match(/\s*$/)[0];
    return lead + v + trail;
  }
  const SKIP = { SCRIPT: 1, STYLE: 1, TEXTAREA: 1, CODE: 1, PRE: 1 };
  const ATTRS = ['placeholder', 'title', 'aria-label'];
  function skipEl(el){ return !el || SKIP[el.nodeName] || (el.closest && el.closest('[data-noi18n],[contenteditable="true"]')); }
  function apply(root){
    if (lang !== 'en' || !root) return;
    try {
      if (root.nodeType === 3) { if (!skipEl(root.parentNode)) { const v = trText(root.nodeValue); if (v !== root.nodeValue) root.nodeValue = v; } return; }
      if (root.nodeType !== 1 && root.nodeType !== 9) return;
      const base = root.nodeType === 9 ? root.body : root; if (!base || skipEl(base)) return;
      const w = document.createTreeWalker(base, NodeFilter.SHOW_TEXT, { acceptNode(n){
        return n.nodeValue && n.nodeValue.trim() && !skipEl(n.parentNode) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT; } });
      const nodes = []; while (w.nextNode()) nodes.push(w.currentNode);
      nodes.forEach(n => { const v = trText(n.nodeValue); if (v !== n.nodeValue) n.nodeValue = v; });
      const els = [base].concat(Array.from(base.querySelectorAll('[placeholder],[title],[aria-label],input[type=button],input[type=submit]')));
      els.forEach(e => {
        ATTRS.forEach(a => { const o = e.getAttribute && e.getAttribute(a); if (o) { const v = trText(o); if (v !== o) e.setAttribute(a, v); } });
        if (e.nodeName === 'INPUT' && (e.type === 'button' || e.type === 'submit') && e.value) { const v = trText(e.value); if (v !== e.value) e.value = v; }
      });
    } catch (e) {}
  }
  let started = false;
  function start(){
    if (started || lang !== 'en' || !document.body) return; started = true;
    try { document.documentElement.lang = 'en'; } catch (e) {}
    if (document.title) document.title = trText(document.title);
    apply(document.body);
    new MutationObserver(ms => { for (const m of ms) {
      if (m.type === 'attributes') { const e = m.target, o = e.getAttribute(m.attributeName); if (o) { const v = trText(o); if (v !== o) e.setAttribute(m.attributeName, v); } continue; }
      if (m.type === 'characterData') { const n = m.target; if (!skipEl(n.parentNode)) { const v = trText(n.nodeValue); if (v !== n.nodeValue) n.nodeValue = v; } continue; }
      m.addedNodes.forEach(nd => apply(nd));
    } }).observe(document.body, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ATTRS });
  }
  function setLang(l){ lang = l === 'en' ? 'en' : 'it'; try { localStorage.setItem('yw_lang', lang); } catch (e) {} location.reload(); }
  /** Selettore IT/EN da mettere in testata o nella pagina di accesso. */
  function select(cls){
    return `<select class="small ${cls || ''}" onchange="ywI18n.setLang(this.value)" title="${lang === 'en' ? 'Language' : 'Lingua'}" aria-label="${lang === 'en' ? 'Language' : 'Lingua'}" data-noi18n data-nocombo style="width:auto">` +
      `<option value="it"${lang === 'it' ? ' selected' : ''}>IT</option><option value="en"${lang === 'en' ? ' selected' : ''}>EN</option></select>`;
  }
  window.ywI18n = { lang: () => lang, tr: trText, apply, start, extend, setLang, select, locale: () => lang === 'en' ? 'en-GB' : 'it-IT' };
  if (lang === 'en') {
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start); else start();
  }
})();
