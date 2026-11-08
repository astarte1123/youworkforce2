#!/usr/bin/env python3
"""Prompt 10 — migrazione legacy: DRY-RUN su fixture dichiarata.

Prova il MECCANISMO di import/riconciliazione idempotente SENZA scrivere su alcun DB:
- normalizza e valida ogni entita,
- deduplica per chiave esterna storica (legacy_id) -> import idempotente,
- risolve i riferimenti (device -> sede),
- separa i record scartati con motivazione (orfani, chiavi/formato invalidi),
- stampa una riconciliazione (letti / da_migrare / scartati / duplicati collassati).

NON dimostra la migrazione di un cliente reale: la fixture e dichiarata e sintetica.

Uso:  python qa/legacy-dryrun.py qa/fixtures/legacy_sample.json
Exit: 0 sempre (un dry-run riporta, non fallisce per record scartati).
"""
import sys, json, re

EMAIL = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")

def dedup(rows, key, kind, rejects):
    """Idempotent keying: first occurrence wins; later same-key rows are collapsed."""
    seen, out, collapsed = {}, [], 0
    for r in rows:
        k = (r.get(key) or "").strip()
        if not k:
            rejects.append((kind, r, "chiave esterna (%s) mancante" % key)); continue
        if k in seen:
            collapsed += 1; continue   # idempotente: stesso legacy_id -> nessun duplicato
        seen[k] = r; out.append(r)
    return out, collapsed

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "qa/fixtures/legacy_sample.json"
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)

    rejects = []
    sites_raw = data.get("sites", [])
    users_raw = data.get("users", [])
    devs_raw  = data.get("devices", [])

    sites, s_col = dedup(sites_raw, "legacy_id", "site", rejects)
    users, u_col = dedup(users_raw, "legacy_id", "user", rejects)
    devs,  d_col = dedup(devs_raw,  "legacy_id", "device", rejects)

    # validate users (email format)
    users_ok = []
    for u in users:
        if not EMAIL.match((u.get("email") or "").strip()):
            rejects.append(("user", u, "email non valida")); continue
        users_ok.append(u)

    # resolve devices -> site (orphans rejected)
    site_ids = {s["legacy_id"] for s in sites}
    devs_ok = []
    for d in devs:
        if not (d.get("external_id") or "").strip():
            rejects.append(("device", d, "external_id mancante")); continue
        if (d.get("site_legacy_id") or "") not in site_ids:
            rejects.append(("device", d, "orfano: sede %r inesistente" % d.get("site_legacy_id"))); continue
        devs_ok.append(d)

    def line(kind, read, migr, col):
        print(f"  {kind:8s} letti={read:2d}  da_migrare={migr:2d}  duplicati_collassati={col}")

    print("== DRY-RUN migrazione (fixture dichiarata, nessuna scrittura su DB) ==")
    print("  sorgente:", path)
    line("sites",   len(sites_raw), len(sites),    s_col)
    line("users",   len(users_raw), len(users_ok), u_col)
    line("devices", len(devs_raw),  len(devs_ok),  d_col)
    print("  -- scartati (con motivazione) --")
    if not rejects:
        print("     nessuno")
    for kind, r, why in rejects:
        rid = r.get("legacy_id") or r.get("external_id") or "?"
        print(f"     [{kind}] {rid}: {why}")

    total_migr = len(sites) + len(users_ok) + len(devs_ok)
    print(f"  == riepilogo: da_migrare={total_migr}, scartati={len(rejects)} ==")
    print("  idempotenza: un secondo run sugli stessi legacy_id non crea duplicati.")
    print("  NB: fixture dichiarata — NON prova la migrazione di un cliente reale.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
