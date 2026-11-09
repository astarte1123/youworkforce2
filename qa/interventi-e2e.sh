#!/usr/bin/env bash
# Fase 3 — Interventi: pianificazione ordini (finestra + priorita') + modifica con concorrenza.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"

JP=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JP" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CP=$(csrfh "$JP"); CM=$(csrfh "$JM")
P(){ curl -s -b "$JP" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }
M(){ curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' "$@"; }
MARIO=$(P "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("mario@")))')

# create con schedule + priorita' + assegnazione
OID=$(P -X POST "$BASE/api/orders" -d "{\"title\":\"Intervento $RUN\",\"assigned_user\":\"$MARIO\",\"scheduled_start\":\"2026-11-14T08:00:00Z\",\"scheduled_end\":\"2026-11-14T12:00:00Z\",\"priority\":3}" | jget 'd["id"]')
[ -n "$OID" ] && ok "ordine pianificato creato ($OID)" || no "create" "$OID"
P "$BASE/api/orders" | python -c "import sys,json;o=next(o for o in json.load(sys.stdin) if o['id']=='$OID');assert o['priority']==3,o;assert o['scheduled_start'],o;assert o['state']=='assegnato',o;print('ok')" >/dev/null 2>&1 \
  && ok "lista: priorita' 3, finestra, stato assegnato" || no "list fields" "-"
VER=$(P "$BASE/api/orders" | python -c "import sys,json;print(next(o['version'] for o in json.load(sys.stdin) if o['id']=='$OID'))")

# edit: titolo + ripianifica + priorita' 1
P -X POST "$BASE/api/orders/$OID/edit" -d "{\"version\":$VER,\"title\":\"Intervento $RUN mod\",\"scheduled_start\":\"2026-11-15T09:00:00Z\",\"scheduled_end\":\"2026-11-15T11:00:00Z\",\"priority\":1}" | grep -q '"ok":true' \
  && ok "modifica ordine (titolo+finestra+priorita')" || no "edit" "-"
P "$BASE/api/orders" | python -c "import sys,json;o=next(o for o in json.load(sys.stdin) if o['id']=='$OID');assert o['priority']==1,o;assert o['title'].endswith('mod'),o;print('ok')" >/dev/null 2>&1 \
  && ok "modifica applicata" || no "edit applied" "-"

# concorrenza: versione vecchia -> 409
C=$(P -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$OID/edit" -d "{\"version\":$VER,\"title\":\"stale\"}")
[ "$C" = "409" ] && ok "versione superata -> 409" || no "concurrency" "$C"

# validazione data
VER2=$(P "$BASE/api/orders" | python -c "import sys,json;print(next(o['version'] for o in json.load(sys.stdin) if o['id']=='$OID'))")
C=$(P -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$OID/edit" -d "{\"version\":$VER2,\"scheduled_start\":\"2026-11-15T12:00:00Z\",\"scheduled_end\":\"2026-11-15T09:00:00Z\"}")
[ "$C" = "400" ] && ok "fine<=inizio -> 400" || no "date validation" "$C"

# clear schedule
P -X POST "$BASE/api/orders/$OID/edit" -d "{\"version\":$VER2,\"clear_schedule\":true}" | grep -q '"ok":true' \
  && P "$BASE/api/orders" | python -c "import sys,json;o=next(o for o in json.load(sys.stdin) if o['id']=='$OID');assert o['scheduled_start'] is None,o;print('ok')" >/dev/null 2>&1 \
  && ok "clear_schedule azzera la finestra" || no "clear_schedule" "-"

# RBAC: lavoratore non modifica
VER3=$(P "$BASE/api/orders" | python -c "import sys,json;print(next(o['version'] for o in json.load(sys.stdin) if o['id']=='$OID'))")
C=$(M -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$OID/edit" -d "{\"version\":$VER3,\"title\":\"hack\"}")
[ "$C" = "403" ] && ok "lavoratore non modifica (403)" || no "rbac edit" "$C"

# cleanup: annulla
P -X POST "$BASE/api/orders/$OID/cancel" -d "{\"version\":$VER3,\"reason\":\"test\"}" >/dev/null

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
