#!/usr/bin/env python3
"""AR-7 (decisione PO 26/11/2026): glossario unico (docs/glossario.md).

Fallisce se nei TESTI visibili della console (stringhe italiane) o nel manuale utente ricompare un
nome vecchio. I codici interni (modulo 'giri', chiavi API, commenti) non contano; le traduzioni
inglesi del dizionario i18n nemmeno (sono il valore a destra dei due punti).
Uso: python qa/glossary-check.py
"""
import re, os, sys
try: sys.stdout.reconfigure(encoding="utf-8")
except Exception: pass
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BANNED = [
    (r"\b[Gg]ir[io]\b", "si dice «percorso di ronda» o «ronda»"),
    (r"\b[Gg]rupp[oi] (di )?allarme\b", "si dice «gruppo di escalation»"),
    (r"\b[Gg]rupp[oi] di reperibili\b", "si dice «gruppo di escalation»"),
    (r"\b[Oo]n-call\b", "si dice «reperibile» / «turno di reperibilità»"),
    (r"\b[Oo]verride\b", "si dice «sostituzione»"),
    (r"\bladder\b", "si dice «sequenza dei canali»"),
]
ALLOW_CTX = [r"'giri'", r'"giri"', r"giri di raccolta"]  # codice del modulo, significato comune

def strip_code(line):
    # toglie commenti JS di riga e le traduzioni EN ("it":"en" -> conta solo la chiave italiana)
    line = re.sub(r"^\s*//.*$", "", line)
    line = re.sub(r'(":)"[^"]*"', r'\1""', line)
    line = re.sub(r'(\b[a-z_]+:)"[^"]*"', r'\1""', line)  # dizionario EN con chiavi senza virgolette
    for a in ALLOW_CTX: line = re.sub(a, "", line)
    return line

def check(path, is_md):
    bad = []
    for n, raw in enumerate(open(path, encoding='utf-8'), 1):
        line = raw if is_md else strip_code(raw)
        if not is_md:
            # solo i testi: tra apici/backtick o tra tag
            texts = re.findall(r">([^<>]+)<|'([^'\\]*(?:\\.[^'\\]*)*)'|\"([^\"]*)\"|`([^`]*)`", line)
            line = " ".join(" ".join(t) for t in texts)
        for pat, why in BANNED:
            for m in re.finditer(pat, line):
                bad.append(f"{os.path.relpath(path, ROOT)}:{n}: «{m.group(0)}» → {why}")
    return bad

files = [(os.path.join(ROOT, 'console', 'static', f), False) for f in ('app.html', 'superadmin.html', 'invite.html', 'worker/index.html')]
files += [(os.path.join(ROOT, 'docs', 'manuale-utente.md'), True), (os.path.join(ROOT, 'docs', 'glossario.md'), None)]
out = []
for p, md in files:
    if md is None or not os.path.exists(p): continue
    out += check(p, md)
for o in out: print(o)
print(f"glossary-check: {len(out)} nomi vecchi")
sys.exit(1 if out else 0)
