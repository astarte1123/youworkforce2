#!/usr/bin/env python3
"""AR-1 (25/11/2026): controllo STATICO delle protezioni di ogni rotta HTTP del core.

Per ogni `.route("/api/...", get(mod::f).post(...))` di core/src/main.rs apre il corpo del gestore
e verifica che contenga:
  - AUTENTICAZIONE: sessione tenant/superadmin, token di ingest o firma;
  - MODULO: il controllo del modulo che la funzione richiede (solo per le rotte tenant);
  - PERMESSO: una capability / regola d'accesso (solo per le scritture tenant).
Le rotte che volutamente non hanno uno dei controlli stanno in qa/handler_guards_allow.txt con il
motivo (una riga: METODO PATH CONTROLLO-MANCANTE # motivo). Una rotta nuova senza controlli e
senza eccezione dichiarata fa fallire il controllo: è il modo per non dimenticarli più.
Uso: python qa/handler_guards.py [--list]   (exit 1 se ci sono rotte scoperte)
"""
import re, sys, os
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, 'core', 'src')
main = open(os.path.join(SRC, 'main.rs'), encoding='utf-8').read()

# corpi delle funzioni per modulo
bodies = {}
for fn in os.listdir(SRC):
    if not fn.endswith('.rs'): continue
    mod = fn[:-3]; txt = open(os.path.join(SRC, fn), encoding='utf-8').read()
    starts = [(m.start(), m.group(1)) for m in re.finditer(r'^(?:pub(?:\(crate\))? )?async fn ([a-zA-Z0-9_]+)\(', txt, re.M)]
    allf = [m.start() for m in re.finditer(r'^(?:pub(?:\(crate\))? )?(?:async )?fn [a-zA-Z0-9_]+[<(]|^#\[|^// ={5,}|^impl |^struct |^pub struct |^enum |^const |^pub const ', txt, re.M)]
    for st, name in starts:
        nxt = [x for x in allf if x > st]
        bodies[(mod, name)] = txt[st:(nxt[0] if nxt else len(txt))]

AUTH = r'WHERE token=\$1|token_hash|esc_verify_inbound|require_tenant|require_super|require_super_or_setup|session_of|resolve_ingest_token|x-ingest-token|ingest_token|verify_twilio|twilio_signature|webhook_secret|lorawan|ingress_auth|cred\b|require_worker|invite_token'
MODULE = r'require_module|require_any_module|module_active|any_module_active|order_view_allowed|order_edit_allowed|ag_safety_scope|ai_feature_on|feature_modules|require_field|guard_incident_site|ESCALATION_MODS|DEVICE_MODS|IOT_MODS|FIELD_MODS|scenario_applied|guard_timesheet_access|guard_owner_editable|order_open'
PERM = r'require_cap|require_any_cap|has_cap\(|is_planner|is_approver|is_supervisor|can_see_costs|require_grantable|guard_[a-z_]+|order_edit_allowed|order_view_allowed|order_line_allowed|is_admin|require_super|require_super_or_setup|\.user_id\s*==|s\.user_id|caps\.contains|can\(|require_role|is_reperibile'

allow = {}
ap = os.path.join(ROOT, 'qa', 'handler_guards_allow.txt')
if os.path.exists(ap):
    for line in open(ap, encoding='utf-8'):
        line = line.split('#', 1)[0].strip()
        if not line: continue
        parts = line.split()
        if len(parts) >= 3: allow.setdefault((parts[0], parts[1]), set()).update(parts[2].split(','))

PUBLIC = {'/api/login', '/api/superadmin/login', '/api/logout', '/api/login-info', '/api/invite/accept', '/api/health', '/health'}
findings = []; total = 0; seen_routes = set()
for m in re.finditer(r'\.route\("(/api/[^"]+)",\s*((?:[a-z]+\([a-zA-Z0-9_:]+\)\s*\.?\s*)+)\)', main):
    path, handlers = m.group(1), m.group(2)
    for hm in re.finditer(r'(get|post|put|delete|patch)\(([a-zA-Z0-9_]+)::([a-zA-Z0-9_]+)\)', handlers):
        meth, mod, fn = hm.group(1).upper(), hm.group(2), hm.group(3)
        total += 1; seen_routes.add((meth, path))
        if path in PUBLIC or path.startswith('/api/_test/'): continue
        body = bodies.get((mod, fn))
        if body is None and mod == 'api':  # gestori spostati in moduli e riesportati da api.rs
            body = next((b for (m2, f2), b in bodies.items() if f2 == fn), None)
        if body is None: findings.append((meth, path, 'handler', f'{mod}::{fn} non trovato')); continue
        superadmin = path.startswith('/api/superadmin/') or 'require_super' in body
        ingress = path.startswith('/api/ingress/')
        need = []
        if not re.search(AUTH, body): need.append('auth')
        if not superadmin and not ingress:
            if not re.search(MODULE, body): need.append('module')
            if meth != 'GET' and not re.search(PERM, body): need.append('perm')
        miss = [n for n in need if n not in allow.get((meth, path), set()) and '*' not in allow.get((meth, path), set())]
        for n in miss: findings.append((meth, path, n, f'{mod}::{fn}'))

# eccezioni per rotte che non esistono più: l'elenco va tenuto onesto
for k in allow:
    if k not in seen_routes: findings.append((k[0], k[1], 'eccezione', 'rotta inesistente in qa/handler_guards_allow.txt'))
if '--list' in sys.argv or findings:
    for f in findings: print(f'SCOPERTA  {f[0]:6} {f[1]:55} manca: {f[2]:7} ({f[3]})')
print(f'handler-guards: {total} rotte, {len(findings)} senza un controllo dichiarato')
sys.exit(1 if findings else 0)
