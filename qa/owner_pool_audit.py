#!/usr/bin/env python3
"""AR-2 (25/11/2026): query sul pool privilegiato (owner_pool, NON soggetto all'RLS) che toccano una
tabella con tenant_id senza filtrare per tenant_id.

Sul pool privilegiato l'isolamento tra aziende dipende SOLO dal filtro scritto nella query: una
query per id senza tenant_id, con un id di un'altra azienda, leggerebbe o modificherebbe i suoi dati.
Ogni query segnalata va corretta (aggiungere `AND tenant_id=$n`) oppure messa in
qa/owner_pool_allow.txt con il motivo (watchdog di sistema su tutti i tenant, token univoci, ...).
Le tabelle con tenant_id si ricavano dalle migrazioni.
Chiave delle eccezioni: funzione:tabelle.
Uso: python qa/owner_pool_audit.py [--list]   (exit 1 se ci sono query scoperte)
"""
import re, os, sys
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, 'core', 'src'); MIG = os.path.join(ROOT, 'core', 'migrations')

# tabelle con tenant_id (dalle migrazioni)
tenant_tables = set(); dropped = set(); renamed = {}
for f in sorted(os.listdir(MIG)):
    sql = open(os.path.join(MIG, f), encoding='utf-8').read()
    for m in re.finditer(r'CREATE TABLE(?: IF NOT EXISTS)?\s+(?:app\.)?([a-z_0-9]+)\s*\((.*?)\n\);', sql, re.S | re.I):
        if re.search(r'\btenant_id\b', m.group(2)): tenant_tables.add(m.group(1))
    for m in re.finditer(r'ALTER TABLE(?: IF EXISTS)?\s+(?:app\.)?([a-z_0-9]+)\s+ADD COLUMN(?: IF NOT EXISTS)?\s+tenant_id', sql, re.I):
        tenant_tables.add(m.group(1))
    for m in re.finditer(r'ALTER TABLE\s+(?:app\.)?([a-z_0-9]+)\s+RENAME TO\s+([a-z_0-9]+)', sql, re.I):
        if m.group(1) in tenant_tables: tenant_tables.add(m.group(2)); dropped.add(m.group(1))
    for m in re.finditer(r'DROP TABLE(?: IF EXISTS)?\s+(?:app\.)?([a-z_0-9]+)', sql, re.I):
        dropped.add(m.group(1))
tenant_tables -= dropped
tenant_tables.discard('tenants')

allow = {}
ap = os.path.join(ROOT, 'qa', 'owner_pool_allow.txt')
if os.path.exists(ap):
    for line in open(ap, encoding='utf-8'):
        k = line.split('#', 1)[0].strip()
        if k: allow[k] = line.split('#', 1)[1].strip() if '#' in line else ''

findings = []; checked = 0
for fn in sorted(os.listdir(SRC)):
    if not fn.endswith('.rs'): continue
    txt = open(os.path.join(SRC, fn), encoding='utf-8').read()
    funcs = [(m.start(), m.group(1)) for m in re.finditer(r'^(?:pub(?:\(crate\))? )?(?:async )?fn ([a-zA-Z0-9_]+)', txt, re.M)]
    for m in re.finditer(r'sqlx::query(?:_scalar|_as)?(?:::<[^>]*>)?\(\s*(?:&format!\()?\s*(r#"(?:.*?)"#|"(?:[^"\\]|\\.)*")', txt, re.S):
        sql = m.group(1)
        # catena fino al ';' (esecutore)
        end = txt.find(';', m.end()); chain = txt[m.end():end if end > 0 else m.end() + 600]
        if 'owner_pool' not in chain and not re.search(r'\(&pool\)|\(&p\)|\(pool\)', chain): continue
        tables = {t for t in re.findall(r'app\.([a-z_0-9]+)', sql) if t in tenant_tables}
        if not tables: continue
        checked += 1
        if re.search(r'\btenant_id\b', sql): continue
        func = [n for (p, n) in funcs if p < m.start()]
        func = func[-1] if func else '?'
        key = f'{func}:{",".join(sorted(tables))}'  # indipendente dal file (gestori spostati tra moduli)
        if key in allow: continue
        line = txt[:m.start()].count('\n') + 1
        findings.append((fn, line, func, sorted(tables), ' '.join(sql.split())[:110]))

if '--list' in sys.argv or findings:
    for f in findings: print(f'SCOPERTA  {f[0]}:{f[1]}  {f[2]}  [{",".join(f[3])}]  {f[4]}')
print(f'owner-pool-audit: {checked} query privilegiate su tabelle di tenant, {len(findings)} senza filtro tenant_id né eccezione')
sys.exit(1 if findings else 0)
