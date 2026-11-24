#!/usr/bin/env bash
# Prompt 8 — app lavoratore: turni server-confirmed + diario operativo (nota voce/manuale).
# La dettatura (STT) è lato browser (Web Speech API) e non è provabile via curl: qui si prova
# il contratto server (turno, nota, idempotenza, gating modulo, isolamento).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
jq_get(){ python -c "import sys,json;print(json.load(sys.stdin).get('$1',''))"; }

M=$(mktemp); G=$(mktemp)
curl -s -c "$M" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"mario@acme.loneworker.eu","password":"demo"}' >/dev/null
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
mario(){ curl -s -b "$M" -H 'x-tenant-slug: acme' -H "x-csrf-token: $(csrfh "$M")" "$@"; }

# clean slate: end any open shift
mario -X POST "$BASE/api/shifts/end" >/dev/null 2>&1 || true

echo "== turni (server-confirmed) =="
R=$(mario -X POST "$BASE/api/shifts/start" -H 'content-type: application/json' -d '{}')
[ "$(echo "$R" | jq_get state)" = "in_corso" ] && ok "inizio turno -> in_corso" || no "start" "$R"
R2=$(mario -X POST "$BASE/api/shifts/start" -H 'content-type: application/json' -d '{}')
[ "$(echo "$R2" | jq_get reused)" = "True" ] && ok "doppio inizio idempotente (reused)" || no "idempotent start" "$R2"
C=$(mario "$BASE/api/shifts/current")
[ "$(echo "$C" | jq_get active)" = "True" ] && ok "turno corrente attivo" || no "current" "$C"

echo "== diario: nota voce + idempotenza =="
IDEM="note-$(date +%s)"
N=$(mario -X POST "$BASE/api/diary" -H 'content-type: application/json' -d "{\"body\":\"Sostituito sensore linea 2, 30 minuti\",\"source\":\"voce\",\"idem_key\":\"$IDEM\"}")
[ "$(echo "$N" | jq_get deduplicated)" = "False" ] && ok "nota creata (source voce)" || no "diary create" "$N"
N2=$(mario -X POST "$BASE/api/diary" -H 'content-type: application/json' -d "{\"body\":\"Sostituito sensore linea 2, 30 minuti\",\"source\":\"voce\",\"idem_key\":\"$IDEM\"}")
[ "$(echo "$N2" | jq_get deduplicated)" = "True" ] && ok "re-invio stessa nota deduplicato" || no "diary dedup" "$N2"
mario -X POST "$BASE/api/diary" -H 'content-type: application/json' -d '{"body":"nota manuale","source":"manuale"}' >/dev/null
L=$(mario "$BASE/api/diary")
echo "$L" | grep -q '"source":"voce"' && ok "lista diario contiene la nota vocale" || no "diary list" "$L"
echo "$L" | grep -q 'nota vuota' && no "empty guard" "accepted empty" || true
EMPTY=$(curl -s -o /dev/null -w "%{http_code}" -b "$M" -H 'x-tenant-slug: acme' -H "x-csrf-token: $(csrfh "$M")" -X POST "$BASE/api/diary" -H 'content-type: application/json' -d '{"body":"   "}')
[ "$EMPTY" = "400" ] && ok "nota vuota rifiutata (400)" || no "empty note" "$EMPTY"

echo "== fine turno =="
E=$(mario -X POST "$BASE/api/shifts/end")
[ "$(echo "$E" | jq_get state)" = "terminato" ] && ok "fine turno -> terminato" || no "end" "$E"
E2=$(curl -s -o /dev/null -w "%{http_code}" -b "$M" -H 'x-tenant-slug: acme' -H "x-csrf-token: $(csrfh "$M")" -X POST "$BASE/api/shifts/end")
[ "$E2" = "409" ] && ok "fine turno senza turno aperto -> 409" || no "double end" "$E2"

echo "== gating modulo (globex: workforce OFF, interventi ON) =="
curl -s -c "$G" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: globex' \
  -d '{"email":"admin@globex.loneworker.eu","password":"demo"}' >/dev/null
# P4: il diario appartiene al Campo = require_any_module(workforce|interventi). globex ha interventi
# attivo -> diario DISPONIBILE (200). Il caso "nessun modulo Campo -> 403" è coperto da
# module-autonomy-e2e.sh (scenario solo-Sicurezza).
GC=$(curl -s -o /dev/null -w "%{http_code}" -b "$G" -H 'x-tenant-slug: globex' -H "x-csrf-token: $(csrfh "$G")" -X POST "$BASE/api/diary" -H 'content-type: application/json' -d '{"body":"x"}')
[ "$GC" = "200" ] && ok "diario disponibile su globex (interventi attivo) -> 200" || no "module gating" "$GC"
GS=$(curl -s -o /dev/null -w "%{http_code}" -b "$G" -H 'x-tenant-slug: globex' -H "x-csrf-token: $(csrfh "$G")" -X POST "$BASE/api/shifts/start" -H 'content-type: application/json' -d '{}')
# Q4 (25/11/2026): il turno serve anche alla sicurezza -> si apre con workforce OPPURE loneworker
GLW=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT count(*) FROM app.tenant_modules m JOIN app.tenants t ON t.id=m.tenant_id WHERE t.slug='globex' AND m.module_code IN ('workforce','loneworker') AND m.state='attivo'" | tr -d '[:space:]')
if [ "$GLW" = "0" ]; then [ "$GS" = "403" ] && ok "turni bloccati su globex (né workforce né loneworker) -> 403" || no "shift gating" "$GS"
else [ "$GS" = "200" ] && ok "turni disponibili su globex (Lone Worker attivo) -> 200" || no "shift gating" "$GS"
  curl -s -o /dev/null -b "$G" -H 'x-tenant-slug: globex' -H "x-csrf-token: $(csrfh "$G")" -X POST "$BASE/api/shifts/end"; fi

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
