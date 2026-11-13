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
  const r = await fetch('/api'+path, {credentials:'include', headers, ...opts});
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
function esc(s){ return String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

function toast(msg, kind=''){ let w=$('#toasts'); if(!w){w=el('<div id="toasts"></div>');document.body.append(w);}
  const t=el(`<div class="toast ${kind}">${ICON[kind==='bad'?'alert':kind==='ok'?'check':'info']||''}<div>${esc(msg)}</div></div>`);
  w.append(t); setTimeout(()=>{t.style.opacity='0';t.style.transition='.3s';setTimeout(()=>t.remove(),300)}, 4200); }

const fmtEUR = c => (c==null?'—':new Intl.NumberFormat('it-IT',{style:'currency',currency:'EUR'}).format(c/100));
const fmtMin = m => { const h=Math.floor(m/60), mm=m%60; return (h?h+'h ':'')+(mm||!h?mm+'m':''); };
const fmtTime = s => new Date(s).toLocaleString('it-IT',{day:'2-digit',month:'2-digit',hour:'2-digit',minute:'2-digit'});
const chip = (s)=>`<span class="chip s-${s}">${esc((s||'').replace(/_/g,' '))}</span>`;
const initials = e => (e||'?').split('@')[0].split(/[.\-_]/).map(x=>x[0]).join('').slice(0,2).toUpperCase();

function theme(){ try{return localStorage.getItem('yw_theme')||''}catch(e){return ''} }
function setTheme(t){ try{localStorage.setItem('yw_theme',t)}catch(e){} applyTheme(); }
function applyTheme(){ const t=theme(); if(t) document.documentElement.setAttribute('data-theme',t); else document.documentElement.removeAttribute('data-theme'); }
function toggleTheme(){ const cur=document.documentElement.getAttribute('data-theme'); setTheme(cur==='dark'?'light':'dark'); }
applyTheme();

const ICON = {
  shield:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 3l7 3v6c0 4-3 7-7 9-4-2-7-5-7-9V6z"/></svg>',
  grid:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="3" y="3" width="7" height="7"/><rect x="14" y="3" width="7" height="7"/><rect x="3" y="14" width="7" height="7"/><rect x="14" y="14" width="7" height="7"/></svg>',
  alert:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/></svg>',
  clipboard:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2"/></svg>',
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
