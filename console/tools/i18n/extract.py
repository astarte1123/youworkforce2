"""Estrae i testi italiani mostrabili all'utente in strings.json (vedi build.py).

Fonti: pagine della console (testo tra i tag, attributi, stringhe letterali JS) e i testi che il core
manda all'interfaccia (stringhe letterali Rust, seed SQL dei cataloghi). Euristica: una frase è
«italiana» se contiene lettere accentate o parole italiane comuni; codici e chiavi sono esclusi.
Le frasi con parti variabili (${...} nelle pagine, {} in format! nel core) diventano modelli {0},{1}.
"""
import glob, html, json, os, re

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..'))
OUT = os.path.join(HERE, 'strings.json')
PAGES = ['console/static/app.html', 'console/static/app.js', 'console/static/superadmin.html',
         'console/static/worker/index.html', 'console/static/invite.html']
IT = re.compile(r"[àèéìòù]|\b(il|lo|la|le|gli|di|del|della|dei|delle|un|una|per|con|non|nessun[oa]?|che|sono|al|alla|dal|dalla|nel|nella|sul|sulla|da|in|su|tra|o|e|ed|se|quando|dove|come|tutti|tutte|ogni|nuov[oa]|salva|elimina|aggiungi|modifica|crea|annulla|chiudi|conferma|cerca|attiv[oa]|scegli|mostra|invia|apri|ore|giorni|minuti|utente|utenti|sede|sedi|dispositiv[oi]|allarm[ei]|gruppo|ronda|ronde|lavoratore|turno|stato|nome|tipo|data|ultimo|ultima|oggi|ieri|creat[oa]|eliminat[oa]|salvat[oa]|impostat[oa]|aggiunt[oi]|rimoss[oi]|assegnat[oa]|registrat[oa]|chius[oa]|aperto|mancat[oa]|errore|richiesta|obbligatori[oa]|valid[oa]|trovat[oa])\b", re.I)


def norm(c):
    return re.sub(r"\s+", " ", html.unescape(c.replace("\\'", "'").replace('\\"', '"').replace('\\n', ' '))).strip()


EN_COMMON = {'none', 'date', 'name', 'type', 'state', 'save', 'delete', 'update', 'create', 'value', 'time', 'file',
             'mode', 'code', 'role', 'site', 'note', 'title', 'image', 'phone', 'active', 'online', 'offline', 'data',
             'size', 'scope', 'source', 'device', 'service', 'profile', 'page', 'table', 'module', 'here', 'there'}


def italianish(s):
    """Parole che finiscono per lo più in vocale (a/e/i/o): l'italiano sì, i codici e l'inglese di rado."""
    words = [w for w in re.findall(r"[A-Za-zÀ-ú']+", s) if len(w) >= 3]
    if not words or any(w.lower() in EN_COMMON for w in words) and len(words) == 1: return False
    return sum(1 for w in words if w[-1].lower() in 'aeioàèéìòù') / len(words) >= 0.6


def good(s):
    if len(s) < 2 or len(s) > 400: return False
    if not re.search(r"[A-Za-zÀ-ú]{2}", s): return False
    if re.search(r"[{};=<>`]|=>|\(\)|function|\$\(|^'\+|\+'$|\bSELECT\b|\bWHERE\b|\bINSERT\b|https?://", s): return False
    if re.fullmatch(r"[A-Za-z0-9_.\-/:#@]+", s) and re.search(r"[_.\-/:#@0-9]", s): return False  # codici e chiavi
    return bool(IT.search(s)) or italianish(s)


phr, tpl = set(), set()
DYN = re.compile(r"\$\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}")


def add_tpl(t):
    lit = re.sub(r"\{\d+\}", "", t)
    if re.search(r"\{\d+\}", t) and good(lit) and re.search(r"[A-Za-zÀ-ú]{3}", lit): tpl.add(t)


def numbered(s, pat):
    n = [0]
    def sub(_): n[0] += 1; return '{%d}' % (n[0] - 1)
    return norm(pat.sub(sub, s))


for f in PAGES:
    src = open(os.path.join(ROOT, f), encoding='utf-8').read()
    cands = []
    for m in re.finditer(r">([^<>`]{2,400})<", src):
        seg = m.group(1); parts = DYN.split(seg); cands.extend(parts)
        if len(parts) > 1:
            t = numbered(seg, DYN)
            if '$' not in t: add_tpl(t)
    for m in re.finditer(r'(?:placeholder|title|aria-label)="([^"$`]{2,300})"', src): cands.append(m.group(1))
    for m in re.finditer(r"'((?:[^'\\\n]|\\.){2,400})'", src): cands.append(m.group(1))      # ogni letterale '...'
    for m in re.finditer(r'"((?:[^"\\\n]|\\.){2,400})"', src): cands.append(m.group(1))      # ogni letterale "..."
    for m in re.finditer(r"`((?:[^`\\<>]|\\.){2,400})`", src):                                # template senza HTML
        seg = m.group(1); parts = DYN.split(seg); cands.extend(parts)
        if len(parts) > 1:
            t = numbered(seg, DYN)
            if '$' not in t: add_tpl(t)
    for c in list(cands):   # letterali con HTML dentro: ogni pezzo di testo tra i tag
        if '<' in c: cands.extend(p for p in re.split(r"<[^>]*>|\$\{[^}]*\}|'\s*\+[^+]*\+\s*'", c) if p.strip())
    for c in cands:
        c = norm(c)
        if good(c): phr.add(c)

# core: letterali Rust (messaggi, etichette, passi guidati) e seed SQL dei cataloghi
FMT = re.compile(r"\{[a-z_0-9]*(?::[^}]*)?\}")
for f in glob.glob(os.path.join(ROOT, 'core', 'src', '*.rs')):
    if f.endswith('main.rs'): continue   # dati dimostrativi (nomi di sedi, persone): sono dati, non interfaccia
    src = open(f, encoding='utf-8').read()
    for m in re.finditer(r'"((?:[^"\\\n]|\\.){3,400})"', src):
        c = norm(m.group(1))
        if FMT.search(c):
            t = numbered(c, FMT)
            if '{{' not in c: add_tpl(t)
            continue
        if good(c): phr.add(c)
for f in glob.glob(os.path.join(ROOT, 'core', 'migrations', '*.sql')):
    src = open(f, encoding='utf-8').read()
    if not re.search(r"INSERT INTO app\.(event_types|plans_catalog|modules|iot_scenarios)", src): continue
    for m in re.finditer(r"'((?:[^'\n]|'')\s*[^'\n]{2,400})'", src):
        c = norm(m.group(1).replace("''", "'"))
        if good(c): phr.add(c)

json.dump({"phrases": sorted(phr), "templates": sorted(tpl)}, open(OUT, 'w', encoding='utf-8'), ensure_ascii=False, indent=0)
print(len(phr), len(tpl))
