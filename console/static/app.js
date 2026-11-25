// Shared helpers for all YouWorkforce pages.
// In produzione il tenant deriva dal SOTTODOMINIO (acme.dominio -> "acme"), come
// farà il prodotto rilasciato. Su localhost nudo si ricade sul campo manuale /
// localStorage (comodità dev). "tenants"/"www" = control plane, nessun tenant.
function hostSlug(){
  try{
    const parts = location.hostname.split('.');
    const first = (parts[0]||'').toLowerCase();
    if(parts.length>=2 && first && !['tenants','www','localhost'].includes(first)) return first;
  }catch(e){}
  return null;
}
const store = {
  get slug(){ const h=hostSlug(); if(h) return h; try{return localStorage.getItem('yw_slug')||'acme'}catch(e){return 'acme'} },
  set slug(v){ try{localStorage.setItem('yw_slug',v)}catch(e){} },
  get slugFromHost(){ return hostSlug()!=null; },
};
// tenant slug header is needed only at /api/login; everything else uses the cookie.
function csrfToken(){ const m=document.cookie.match(/(?:^|;\s*)yw_csrf=([^;]+)/); return m?decodeURIComponent(m[1]):''; }
async function api(path, opts={}){
  const headers = {'content-type':'application/json','x-tenant-slug':store.slug, ...(opts.headers||{})};
  const method=(opts.method||'GET').toUpperCase();
  if(!['GET','HEAD','OPTIONS'].includes(method)){ const t=csrfToken(); if(t) headers['x-csrf-token']=t; }
  let r;
  try{ r = await fetch('/api'+path, {credentials:'include', headers, ...opts}); }
  catch(ne){ const e=new Error('Failed to fetch'); e.status=0; e.network=true; e.cause=ne; throw e; }
  let body=null; const ct=r.headers.get('content-type')||'';
  if(ct.includes('json')) body = await r.json().catch(()=>null);
  else body = await r.text().catch(()=>null);
  if(!r.ok){ const msg=(body&&body.error)||(typeof body==='string'&&body)||('HTTP '+r.status);
    const e=new Error(msg); e.status=r.status; e.body=body; throw e; }
  return body;
}
const $  = (s,r=document)=>r.querySelector(s);
const $$ = (s,r=document)=>[...r.querySelectorAll(s)];
function el(html){ const t=document.createElement('template'); t.innerHTML=html.trim(); return t.content.firstElementChild; }
function esc(s){ return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
// IO-2 (25/11/2026): oggetto da passare a un gestore inline (onclick='f(${jattr(o)})').
// Escape HTML COMPLETO del JSON: il browser lo decodifica e il JS riceve esattamente il JSON.
// Prima si sostituiva solo l'apice: un dato con &quot; usciva dalla stringa ed eseguiva codice.
function jattr(o){ return esc(JSON.stringify(o)); }
// Ripiego per codici tecnici senza etichetta (regola PO: mai codici grezzi in UI):
// «order_complete» → «Order complete». Le pagine usano prima le proprie mappe *_LABELS.
function humanizeCode(k){ const t=String(k??'').replace(/[_.:-]+/g,' ').replace(/\s+/g,' ').trim(); return t?t.charAt(0).toUpperCase()+t.slice(1):''; }

// FE-11: errori comprensibili. Il testo del server si mostra solo se è già una frase leggibile
// (ha spazi, nessun SQL/stack/sigla tecnica); altrimenti si traduce per codice HTTP.
function looksTechnical(m){
  const s=String(m??'').trim();
  if(!s||s.length>300) return true;
  if(/^HTTP \d+$/.test(s)) return true;
  if(!/\s/.test(s)) return true; // codice singolo: "db", "forbidden", "not_found"…
  return /sqlx|error returned from database|database|SQLSTATE|violates|constraint|duplicate key|relation "|column "|syntax error|panick|stack|traceback|deserializ|serde|invalid type|missing field|unknown variant|expected .* at line|unexpected token|json|failed to fetch|networkerror|load failed|typeerror|referenceerror|cannot read propert|is not a function|is not defined|bundefinedb|bnullb|bNaNb|<html|<!doctype|internal server error|econn|\bat [\w.$]+ \(|::|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|^(bad|no|missing|invalid|not|unauthorized|forbidden|tenant not)\b/i.test(s);
}
function _cap(s){ s=String(s||'').trim(); return s?s.charAt(0).toUpperCase()+s.slice(1):s; }
function friendlyError(e){
  if(e==null) return 'Operazione non riuscita: riprova';
  const raw=String(typeof e==='string'?e:(e.message||'')).trim();
  let st=(typeof e==='object'&&e)?(Number(e.status)||0):0;
  const hm=/^HTTP (\d{3})$/.exec(raw); if(!st&&hm) st=Number(hm[1]);
  const human=!looksTechnical(raw);
  if((typeof e==='object'&&e&&e.network)||/failed to fetch|networkerror|load failed|network request failed/i.test(raw))
    return 'Connessione al server non riuscita: riprova tra poco';
  if(st===401) return 'Sessione scaduta: accedi di nuovo';
  if(st>=500||/internal server error|sqlx|error returned from database/i.test(raw)||raw==='db')
    return 'Errore del server: l’operazione non è stata completata';
  if(st===403) return human?_cap(raw):'Non hai il permesso per questa operazione';
  if(st===404) return human?_cap(raw):'Elemento non trovato (forse è stato eliminato)';
  if(st===409) return human?_cap(raw):'Operazione in conflitto con una modifica recente: ricarica e riprova';
  if(st===413) return 'File o dati troppo grandi per essere inviati';
  if(st===429) return 'Troppe richieste ravvicinate: attendi qualche secondo e riprova';
  if(human) return _cap(raw);
  if(st>=400) return 'Operazione non riuscita: controlla i dati inseriti e riprova';
  return 'Operazione non riuscita: riprova';
}
// Errori della pagina di accesso: 401 = credenziali errate (non "sessione scaduta").
function loginErrorText(e){
  const st=(e&&e.status)||0, raw=String((e&&e.message)||'');
  if(st===401||(st===400&&/credential/i.test(raw))) return 'Credenziali non valide';
  if(st===403&&!looksTechnical(raw)) return _cap(raw);
  if(st===404&&/tenant/i.test(raw)) return 'Azienda non trovata: controlla il nome del tenant';
  return friendlyError(e);
}
// FE-12: la pagina di accesso mostra password e accessi rapidi di prova SOLO in ambiente demo.
// Se l'informazione non arriva si assume produzione (nessun dato di prova visibile).
async function loginInfo(){
  try{ const r=await fetch('/api/login-info',{credentials:'omit'}); if(!r.ok) return {demo:false};
    const j=await r.json().catch(()=>null); return {demo:!!(j&&j.demo===true)}; }catch(e){ return {demo:false}; }
}
// FE-11b: stato di errore per un caricamento principale (mai mostrato come «nessun dato»).
// retry: nome di funzione globale o espressione JS da eseguire al click su «Riprova».
function errorBox(e, retry){
  const msg=friendlyError(e);
  return `<div class="card pad load-err" role="alert" style="border-left:3px solid var(--bad)">
    <div class="row" style="gap:var(--s-8,8px);align-items:center;flex-wrap:wrap">${ICON.alert||''}
      <div class="grow"><b>Impossibile caricare i dati</b><div class="small mut">${esc(msg)}</div></div>
      ${retry?`<button class="btn ghost small" onclick="${esc(retry)}">Riprova</button>`:''}</div></div>`;
}
function errorRow(colspan, e, retry){
  return `<tr><td colspan="${Number(colspan)||1}" style="padding:0">${errorBox(e,retry)}</td></tr>`;
}

function toast(msg, kind=''){ let w=$('#toasts'); if(!w){w=el('<div id="toasts"></div>');document.body.append(w);}
  // Error passato direttamente → messaggio comprensibile; stringhe tecniche negli avvisi d'errore → idem.
  if(msg&&typeof msg==='object') msg=friendlyError(msg);
  else if(kind==='bad'&&/sqlx|error returned from database|SQLSTATE|failed to fetch|networkerror|load failed|^HTTP \d+$|^db$|<html|<!doctype|internal server error|panick|traceback/i.test(String(msg??'').trim())) msg=friendlyError(String(msg));
  const t=el(`<div class="toast ${kind}">${ICON[kind==='bad'?'alert':kind==='ok'?'check':'info']||''}<div>${esc(msg)}</div></div>`);
  w.append(t); setTimeout(()=>{t.style.opacity='0';t.style.transition='.3s';setTimeout(()=>t.remove(),300)}, 4200); }

const fmtEUR = c => (c==null?'—':new Intl.NumberFormat('it-IT',{style:'currency',currency:'EUR'}).format(c/100));
const fmtMin = m => { const h=Math.floor(m/60), mm=m%60; return (h?h+'h ':'')+(mm||!h?mm+'m':''); };
const fmtTime = s => new Date(s).toLocaleString('it-IT',{day:'2-digit',month:'2-digit',hour:'2-digit',minute:'2-digit'});
// FE-5: etichette degli stati mostrati con chip() (incidenti, interventi, consuntivi, turni,
// check-in, moduli, comandi). Valori reali dal core (migrations/0003, order_transition, ecc.).
const STATE_LABELS={
  // interventi
  bozza:'Bozza',assegnato:'Assegnato',in_corso:'In corso',completato:'Completato',annullato:'Annullato',sospeso:'Sospeso',
  // incidenti / allarmi
  aperto:'Aperto',preso_in_carico:'Preso in carico',chiuso:'Chiuso',escalato:'In escalation',
  // consuntivi
  in_revisione:'In revisione',da_correggere:'Da correggere',approvato:'Approvato',
  // turni, check-in, piani
  pianificato:'Pianificato',confermato:'Confermato',attivo:'Attivo',scaduto:'Scaduto',terminato:'Terminato',
  // dispositivi / moduli
  in_servizio:'In servizio',in_attesa:'In attesa',fuori_servizio:'Fuori servizio',non_abilitato:'Non abilitato',
  in_disattivazione:'In disattivazione',da_configurare:'Da configurare',degradato:'Funziona con limitazioni',
  // comandi, code, esiti
  pending:'In coda',sent:'Inviato',in_flight:'In invio',done:'Eseguito',acked:'Confermato',failed:'Non riuscito',error:'Non riuscito',
  ok:'OK',down:'Non raggiungibile',incompleto:'Incompleto',completata:'Completata',annullata:'Annullata',chiusa:'Chiusa',aperta:'Aperta'};
function stateLabel(s){ return STATE_LABELS[s]||humanizeCode(s)||'—'; }
const chip = (s)=>`<span class="chip s-${String(s||'').replace(/[^a-z0-9_-]/gi,'')}">${esc(stateLabel(s))}</span>`;
const initials = e => (e||'?').split('@')[0].split(/[.\-_]/).map(x=>x[0]).join('').slice(0,2).toUpperCase();

function theme(){ try{return localStorage.getItem('yw_theme')||''}catch(e){return ''} }
function setTheme(t){ try{localStorage.setItem('yw_theme',t)}catch(e){} applyTheme(); }
function applyTheme(){ const t=theme(); if(t) document.documentElement.setAttribute('data-theme',t); else document.documentElement.removeAttribute('data-theme'); syncThemeBtn(); }
function toggleTheme(){ const cur=document.documentElement.getAttribute('data-theme'); setTheme(cur==='dark'?'light':'dark'); }
// Icona del toggle tema: luna col tema chiaro, sole col tema scuro.
function isDarkTheme(){ const a=document.documentElement.getAttribute('data-theme'); if(a==='dark')return true; if(a==='light')return false; try{return matchMedia('(prefers-color-scheme:dark)').matches;}catch(e){return false;} }
function themeIconHtml(){ return isDarkTheme()?ICON.sun:ICON.moon; }
function syncThemeBtn(){ try{ const b=document.getElementById('themeBtn'); if(b) b.innerHTML=themeIconHtml(); }catch(e){} }
try{ matchMedia('(prefers-color-scheme:dark)').addEventListener('change',syncThemeBtn); }catch(e){}
applyTheme();

const ICON = {
  shield:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 3l7 3v6c0 4-3 7-7 9-4-2-7-5-7-9V6z"/></svg>',
  grid:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="3" y="3" width="7" height="7"/><rect x="14" y="3" width="7" height="7"/><rect x="3" y="14" width="7" height="7"/><rect x="14" y="14" width="7" height="7"/></svg>',
  alert:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/></svg>',
  clipboard:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2"/></svg>',
  building:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 21h18M6 21V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v16"/><path d="M9 7h2M13 7h2M9 11h2M13 11h2M9 15h2M13 15h2"/></svg>',
  hardhat:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 18h20"/><path d="M4 18a8 8 0 0 1 16 0"/><path d="M10 6.5V4a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v2.5"/><path d="M7.5 8.3A8 8 0 0 1 10 6.6M16.5 8.3A8 8 0 0 0 14 6.6"/></svg>',
  map:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 3 3 5.5v15L9 18l6 3 6-2.5v-15L15 6 9 3z"/><path d="M9 3v15M15 6v15"/></svg>',
  floorplan:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="1"/><path d="M3 10h8M11 3v7M11 14v7M11 14h10"/></svg>',
  route:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="6" cy="19" r="3"/><circle cx="18" cy="5" r="3"/><path d="M9 19h6a3 3 0 0 0 3-3V8"/><path d="M6 16V8a3 3 0 0 1 3-3h0"/></svg>',
  receipt:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 2v20l3-2 3 2 3-2 3 2 3-2V2l-3 2-3-2-3 2-3-2z"/><path d="M8 8h8M8 12h8"/></svg>',
  sliders:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><circle cx="4" cy="12" r="2"/><circle cx="12" cy="10" r="2"/><circle cx="20" cy="14" r="2"/></svg>',
  users:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75"/></svg>',
  check:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6 9 17l-5-5"/></svg>',
  info:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4m0-4h.01"/></svg>',
  x:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6 6 18M6 6l12 12"/></svg>',
  sos:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>',
  logout:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9"/></svg>',
  moon:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21 12.8A9 9 0 1 1 11.2 3 7 7 0 0 0 21 12.8z"/></svg>',
  home:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 10 12 3l9 7v10a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z"/></svg>',
  doc:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6M9 13h6M9 17h6"/></svg>',
  mic:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10a7 7 0 0 0 14 0M12 19v3"/></svg>',
  play:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polygon points="6 4 20 12 6 20"/></svg>',
  stop:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="5" y="5" width="14" height="14" rx="2"/></svg>',
  clock:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>',
  sun:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>',
  calendar:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="3" y="4" width="18" height="18" rx="2"/><path d="M16 2v4M8 2v4M3 10h18"/></svg>',
  logomark:'<img src="/youworkforce.webp" alt="YouWorkforce" style="width:100%;height:100%;object-fit:contain;display:block">',
};

/* ============ Combo ricercabile su TUTTA la piattaforma (app + superadmin) ============
   Ogni <select> nativo viene potenziato con una casella di ricerca, mantenendo il <select> come
   sorgente di verità: value/onchange/change restano invariati. Self-contained (nessuna dipendenza),
   CSS iniettato via JS (robusto vs cache), auto-applicato via MutationObserver a ogni render. */
function enhanceSelect(sel){
  if(!sel || sel.dataset.combo || sel.multiple || sel.hasAttribute('data-nocombo')) return;
  sel.dataset.combo='1';
  const wrap=document.createElement('span'); wrap.className='combo '+(sel.className||'');
  if(sel.style.width) wrap.style.width=sel.style.width;
  const inp=document.createElement('input'); inp.type='text'; inp.className='combo-input';
  inp.autocomplete='off'; inp.spellcheck=false; inp.setAttribute('role','combobox');
  if(sel.disabled)inp.disabled=true; if(sel.title)inp.title=sel.title;
  const panel=document.createElement('div'); panel.className='combo-panel'; panel.hidden=true;
  sel.parentNode.insertBefore(wrap, sel);
  wrap.appendChild(inp); wrap.appendChild(panel); wrap.appendChild(sel);
  sel.style.display='none';
  const curLabel=()=>{ const o=sel.options[sel.selectedIndex]; return o?o.text:''; };
  inp.value=curLabel();
  let hi=-1, items=[];
  function render(filter){
    const toks=(filter||'').trim().toLowerCase().split(/\s+/).filter(Boolean);
    items=Array.from(sel.options).filter(o=>{ const t=o.text.toLowerCase(); return toks.every(k=>t.includes(k)); });
    panel.textContent='';
    if(!items.length){ const e=document.createElement('div'); e.className='combo-empty'; e.textContent='nessun risultato'; panel.appendChild(e); return; }
    items.forEach((o,i)=>{ const d=document.createElement('div'); d.className='combo-opt'+(o.value===sel.value?' sel':''); d.dataset.i=i;
      if(o.disabled)d.classList.add('dis'); d.textContent=o.text; panel.appendChild(d); });
    hi=items.findIndex(o=>o.value===sel.value); if(hi<0)hi=0; paintHi();
  }
  function paintHi(){ Array.from(panel.children).forEach((c,i)=>c.classList.toggle('hi', i===hi)); }
  function scrollHi(){ const el=panel.children[hi]; if(el&&el.scrollIntoView)el.scrollIntoView({block:'nearest'}); }
  function open(){ if(!panel.hidden||sel.disabled)return; render(''); panel.hidden=false; wrap.classList.add('open'); setTimeout(()=>inp.select(),0); }
  function close(restore){ panel.hidden=true; wrap.classList.remove('open'); if(restore!==false) inp.value=curLabel(); }
  function pick(o){ if(!o||o.disabled)return; if(sel.value!==o.value){ sel.value=o.value; sel.dispatchEvent(new Event('change',{bubbles:true})); sel.dispatchEvent(new Event('input',{bubbles:true})); } inp.value=o.text; close(false); }
  inp.addEventListener('focus',open);
  inp.addEventListener('mousedown',()=>{ if(panel.hidden) setTimeout(open,0); });
  inp.addEventListener('input',()=>{ if(panel.hidden){ panel.hidden=false; wrap.classList.add('open'); } render(inp.value); });
  inp.addEventListener('keydown',e=>{
    if(e.key==='ArrowDown'){ e.preventDefault(); if(panel.hidden){open();} else { hi=Math.min(hi+1,items.length-1); paintHi(); scrollHi(); } }
    else if(e.key==='ArrowUp'){ e.preventDefault(); if(!panel.hidden){ hi=Math.max(hi-1,0); paintHi(); scrollHi(); } }
    else if(e.key==='Enter'){ if(!panel.hidden){ e.preventDefault(); pick(items[hi]); } }
    else if(e.key==='Escape'){ if(!panel.hidden){ e.preventDefault(); close(true); inp.blur(); } }
    else if(e.key==='Tab'){ close(true); }
  });
  panel.addEventListener('mousedown',e=>{ const d=e.target.closest('.combo-opt'); if(d){ e.preventDefault(); pick(items[+d.dataset.i]); } });
  document.addEventListener('mousedown',e=>{ if(!wrap.contains(e.target)) close(true); });
  sel.addEventListener('change',()=>{ if(document.activeElement!==inp) inp.value=curLabel(); });
}
function enhanceAllSelects(root){ try{ (root||document).querySelectorAll('select:not([data-combo])').forEach(s=>{ try{ enhanceSelect(s); }catch(e){} }); }catch(e){} }
let _comboT=null;
function initComboUI(){
  if(!document.getElementById('comboCss')){
    const st=document.createElement('style'); st.id='comboCss'; st.textContent=`
    .combo{position:relative;display:flex;width:100%;min-width:0;max-width:100%;vertical-align:top}
    .combo.open{z-index:3000}
    .combo.grow{flex:1 1 auto}
    .combo-input{width:100%;min-width:0;font:inherit;cursor:text;
      background-image:linear-gradient(45deg,transparent 50%,var(--faint) 50%),linear-gradient(135deg,var(--faint) 50%,transparent 50%);
      background-position:calc(100% - 16px) 55%,calc(100% - 11px) 55%;background-size:5px 5px,5px 5px;background-repeat:no-repeat;padding-right:34px}
    .combo-panel{position:absolute;z-index:1200;top:calc(100% + 2px);left:0;min-width:100%;width:max-content;max-width:min(360px,90vw);
      max-height:260px;overflow:auto;background:var(--surface,#fff);border:1px solid var(--line);border-radius:8px;
      box-shadow:var(--sh-lg,0 8px 28px rgba(0,0,0,.22));padding:4px}
    .combo-opt{padding:6px 9px;border-radius:6px;cursor:pointer;font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
    .combo-opt.hi{background:var(--surface2,#eef2ff)}
    .combo-opt.sel{font-weight:600}
    .combo-opt.dis{opacity:.5;cursor:not-allowed}
    .combo-empty{padding:8px 9px;color:var(--mut,#888);font-size:12px}`;
    document.head.appendChild(st);
  }
  enhanceAllSelects(document);
  const mo=new MutationObserver(muts=>{ for(const m of muts){ if(m.addedNodes&&m.addedNodes.length){ if(!_comboT)_comboT=setTimeout(()=>{_comboT=null;enhanceAllSelects(document);},40); break; } } });
  if(document.body) mo.observe(document.body,{childList:true,subtree:true});
}
if(document.readyState!=='loading') initComboUI();
else document.addEventListener('DOMContentLoaded', initComboUI);

// ===== Drag & drop universale su OGNI <input type=file> =====
// Trascinando file/immagini sul contenitore dell'input (label/field/row) vengono assegnati
// all'input e parte l'evento change, esattamente come con "Sfoglia". Vale per tutti gli input
// file presenti e futuri (drawer, form dinamici), su app.html e superadmin.html.
function armDropzones(root){
  (root||document).querySelectorAll('input[type=file]:not([data-dnd])').forEach(inp=>{
    inp.dataset.dnd='1';
    const zone=inp.closest('label')||inp.closest('.field')||inp.closest('.row')||inp.parentElement||inp;
    if(zone.dataset.dz) return; zone.dataset.dz='1';
    zone.title=(zone.title?zone.title+' · ':'')+'Trascina qui il file, oppure usa Sfoglia';
    const on=()=>{ zone.style.outline='2px dashed var(--brand,#e2001a)'; zone.style.outlineOffset='3px'; zone.style.borderRadius=zone.style.borderRadius||'8px'; };
    const off=()=>{ zone.style.outline=''; zone.style.outlineOffset=''; };
    zone.addEventListener('dragenter',e=>{ e.preventDefault(); on(); });
    zone.addEventListener('dragover',e=>{ e.preventDefault(); e.dataTransfer.dropEffect='copy'; on(); });
    zone.addEventListener('dragleave',e=>{ if(!zone.contains(e.relatedTarget)) off(); });
    zone.addEventListener('drop',e=>{
      e.preventDefault(); off();
      const files=e.dataTransfer&&e.dataTransfer.files; if(!files||!files.length) return;
      // rispetta single/multiple e il filtro accept (se presente)
      const acc=(inp.getAttribute('accept')||'').split(',').map(s=>s.trim().toLowerCase()).filter(Boolean);
      const okType=f=>!acc.length||acc.some(a=>{
        if(a==='image/*') return f.type.startsWith('image/');
        if(a.endsWith('/*')) return f.type.startsWith(a.slice(0,-1));
        if(a.startsWith('.')) return f.name.toLowerCase().endsWith(a);
        return f.type.toLowerCase()===a; });
      let list=Array.from(files).filter(okType);
      if(!list.length){ try{toast&&toast('Tipo di file non supportato qui','bad');}catch(_){} return; }
      if(!inp.multiple) list=[list[0]];
      try{ const dt=new DataTransfer(); list.forEach(f=>dt.items.add(f)); inp.files=dt.files; }
      catch(_){ try{ inp.files=files; }catch(__){ return; } }
      inp.dispatchEvent(new Event('change',{bubbles:true}));
    });
  });
}
(function(){
  const run=()=>armDropzones(document);
  if(document.readyState!=='loading') run(); else document.addEventListener('DOMContentLoaded',run);
  let t=null;
  const mo=new MutationObserver(ms=>{ for(const m of ms){ if(m.addedNodes&&m.addedNodes.length){ if(!t)t=setTimeout(()=>{t=null;armDropzones(document);},40); break; } } });
  if(document.body) mo.observe(document.body,{childList:true,subtree:true});
  else document.addEventListener('DOMContentLoaded',()=>mo.observe(document.body,{childList:true,subtree:true}));
})();
