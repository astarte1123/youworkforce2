#!/usr/bin/env bash
# Fase 2 — forza lavoro + turni: squadre, turni pianificati (conferma/annulla), reperibilita'.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"

JA=$(mktemp); JP=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JP" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CP=$(csrfh "$JP"); CM=$(csrfh "$JM")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
P(){ curl -s -b "$JP" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }
M(){ curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' "$@"; }
MARIO=$(A "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("mario@")))')

# --- Squadre ---
TEAM=$(A -X POST "$BASE/api/teams" -d "{\"name\":\"Squadra $RUN\"}" | jget 'd["id"]')
[ -n "$TEAM" ] && ok "squadra creata ($TEAM)" || no "create_team" "$TEAM"
A -X POST "$BASE/api/teams/$TEAM/members" -d "{\"user_id\":\"$MARIO\"}" | grep -q '"ok":true' && ok "membro aggiunto a squadra" || no "add_team_member" "-"
A "$BASE/api/teams" | python -c "import sys,json;d=json.load(sys.stdin);assert any(t['id']=='$TEAM' and t['members'] for t in d);print('ok')" >/dev/null 2>&1 && ok "squadra con membri in lista" || no "list_teams" "-"

# --- Turni pianificati ---
SP=$(P -X POST "$BASE/api/shift-plans" -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"2026-11-10T08:00:00Z\",\"ends_at\":\"2026-11-10T16:00:00Z\",\"note\":\"cantiere A\"}" | jget 'd["id"]')
[ -n "$SP" ] && ok "turno pianificato creato ($SP)" || no "create_shift_plan" "$SP"
P "$BASE/api/shift-plans" | grep -q "$SP" && ok "pianificatore vede il turno" || no "list planner" "-"
M "$BASE/api/shift-plans" | grep -q "$SP" && ok "lavoratore vede il proprio turno" || no "list worker own" "-"
# validazione: fine<=inizio
C=$(P -o /dev/null -w '%{http_code}' -X POST "$BASE/api/shift-plans" -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"2026-11-10T16:00:00Z\",\"ends_at\":\"2026-11-10T08:00:00Z\"}")
[ "$C" = "400" ] && ok "fine<=inizio -> 400" || no "date validation" "$C"
# RBAC: lavoratore non pianifica
C=$(M -o /dev/null -w '%{http_code}' -X POST "$BASE/api/shift-plans" -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"2026-11-11T08:00:00Z\",\"ends_at\":\"2026-11-11T16:00:00Z\"}")
[ "$C" = "403" ] && ok "lavoratore non pianifica (403)" || no "rbac shift" "$C"
# lavoratore conferma il PROPRIO turno
M -X POST "$BASE/api/shift-plans/$SP" -d '{"action":"confirm"}' | grep -q '"ok":true' && ok "lavoratore conferma turno" || no "confirm" "-"
M "$BASE/api/shift-plans" | python -c "import sys,json;d=json.load(sys.stdin);print(next(p['state'] for p in d if p['id']=='$SP'))" | grep -q confermato && ok "stato -> confermato" || no "state confermato" "-"
# pianificatore annulla
P -X POST "$BASE/api/shift-plans/$SP" -d '{"action":"cancel"}' | grep -q '"ok":true' && ok "pianificatore annulla" || no "cancel" "-"

# --- Reperibilita' ---
OC=$(P -X POST "$BASE/api/oncall" -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"2026-11-12T20:00:00Z\",\"ends_at\":\"2026-11-13T06:00:00Z\",\"note\":\"notte\"}" | jget 'd["id"]')
[ -n "$OC" ] && ok "reperibilita' creata ($OC)" || no "create_oncall" "$OC"
P "$BASE/api/oncall" | grep -q "$OC" && ok "reperibilita' in lista" || no "list_oncall" "-"
P -X DELETE "$BASE/api/oncall/$OC" | grep -q '"ok":true' && ok "reperibilita' eliminata" || no "delete_oncall" "-"

# cleanup
A -X DELETE "$BASE/api/teams/$TEAM" >/dev/null
P -X DELETE "$BASE/api/shift-plans/$SP" >/dev/null

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
