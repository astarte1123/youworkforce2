#!/usr/bin/env bash
# Configurazione guidata: GET /api/setup-status calcola la checklist "cosa manca" dallo stato reale
# del tenant, condizionata ai moduli attivi, SOLO per tenant_admin. Tenant di prova: acme.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }

# admin del tenant
JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
R=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/setup-status")
echo "$R" | grep -q '"items"' && ok "admin: setup-status risponde con items" || { no "nessun items" "$R"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }

TOTAL=$(echo "$R" | jv 'd["total"]'); DONE=$(echo "$R" | jv 'd["done"]')
[ "${TOTAL:-0}" -ge 1 ] && ok "almeno 1 passo calcolato (total=$TOTAL)" || no "nessun passo" "$TOTAL"
[ "${DONE:-x}" -le "${TOTAL:-0}" ] && ok "done ($DONE) <= total ($TOTAL)" || no "done>total" "-"

# ogni item ha i campi attesi
echo "$R" | jv 'all(all(k in it for k in ["key","done","title","why","link","cta"]) for it in d["items"])' | grep -qi true \
  && ok "ogni passo ha key/done/title/why/link/cta" || no "campi passo mancanti" "-"

# 'ready' coerente con done==total
echo "$R" | jv 'd["ready"]==(d["done"]==d["total"])' | grep -qi true && ok "flag ready coerente" || no "ready incoerente" "-"

# item condizionati ai moduli: 'devices' presente solo se un modulo dispositivi è attivo (acme ha loneworker/iot)
echo "$R" | jv '"devices" in [it["key"] for it in d["items"]]' | grep -qi true && ok "item condizionati ai moduli (devices presente per acme)" || no "condizione moduli non applicata" "-"

# GATING: un ruolo non-admin NON può vedere la configurazione
JO=$(mktemp)
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/setup-status")
[ "$CODE" = "403" ] && ok "operatore: 403 (solo tenant_admin)" || no "gating admin mancante" "$CODE"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
