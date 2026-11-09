#!/usr/bin/env bash
# Fase 1 — superfici di gestione: dispositivi, sedi, utenti, gruppi allarme+routing,
# assegnazione/note incidenti, check-in in centrale. RBAC tenant_admin.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"

JA=$(mktemp); JO=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CO=$(csrfh "$JO"); CM=$(csrfh "$JM")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
O(){ curl -s -b "$JO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CO" -H 'content-type: application/json' "$@"; }
M(){ curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' "$@"; }

# --- Sedi ---
SID=$(A -X POST "$BASE/api/sites" -d "{\"name\":\"Cantiere $RUN\"}" | jget 'd["id"]')
[ -n "$SID" ] && ok "sede creata ($SID)" || no "create_site" "$SID"
A "$BASE/api/sites" | grep -q "$SID" && ok "sede in lista" || no "list_sites" "-"
A -X POST "$BASE/api/sites/$SID" -d "{\"name\":\"Cantiere $RUN b\"}" | grep -q '"ok":true' && ok "sede rinominata" || no "update_site" "-"

# --- Dispositivi ---
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"DEV-$RUN\",\"source\":\"manual\",\"label\":\"Badge test\",\"site_id\":\"$SID\"}" | jget 'd["id"]')
[ -n "$DID" ] && ok "dispositivo creato ($DID)" || no "create_device" "$DID"
A "$BASE/api/devices" | grep -q "DEV-$RUN" && ok "dispositivo in lista con sede" || no "list_devices" "-"
A -X POST "$BASE/api/devices/$DID" -d "{\"external_id\":\"DEV-$RUN\",\"source\":\"manual\",\"label\":\"Badge test\",\"enabled\":false}" | grep -q '"ok":true' && ok "dispositivo disabilitato" || no "update_device" "-"
# RBAC: operatore non puo creare dispositivi
C=$(O -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices" -d "{\"external_id\":\"x-$RUN\",\"source\":\"manual\"}")
[ "$C" = "403" ] && ok "operatore non crea dispositivi (403)" || no "rbac device" "$C"

# --- Utenti ---
UEMAIL="temp-$RUN@acme.loneworker.eu"
UUID_NEW=$(A -X POST "$BASE/api/users" -d "{\"email\":\"$UEMAIL\",\"role\":\"operatore\",\"password\":\"segreta1\"}" | jget 'd["id"]')
[ -n "$UUID_NEW" ] && ok "utente creato ($UEMAIL)" || no "create_user" "$UUID_NEW"
# il nuovo utente puo autenticarsi
C=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$UEMAIL\",\"password\":\"segreta1\"}")
[ "$C" = "200" ] && ok "nuovo utente fa login" || no "new user login" "$C"
A -X POST "$BASE/api/users/$UUID_NEW" -d '{"role":"auditor"}' | grep -q '"ok":true' && ok "ruolo utente aggiornato" || no "update_user" "-"
A -X DELETE "$BASE/api/users/$UUID_NEW" | grep -q '"ok":true' && ok "utente eliminato" || no "delete_user" "-"

# --- Gruppi allarme + routing ---
GID=$(A -X POST "$BASE/api/groups" -d "{\"name\":\"Reperibili $RUN\"}" | jget 'd["id"]')
[ -n "$GID" ] && ok "gruppo allarme creato ($GID)" || no "create_group" "$GID"
MARIO=$(A "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("mario@")))')
A -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$MARIO\",\"position\":1}" | grep -q '"ok":true' && ok "membro aggiunto al gruppo" || no "add_member" "-"
A -X POST "$BASE/api/routing" -d "{\"etype\":\"sos\",\"group_id\":\"$GID\"}" | grep -q '"ok":true' && ok "routing sos->gruppo impostato" || no "set_routing" "-"
A "$BASE/api/groups" | python -c "import sys,json;d=json.load(sys.stdin);assert any(g['id']=='$GID' and g['members'] for g in d['groups']);assert any(r['etype']=='sos' for r in d['routing']);print('ok')" >/dev/null 2>&1 \
  && ok "gruppi+routing esposti" || no "list_groups" "-"

# --- Incidenti: assegnazione + note ---
IID=$(M -X POST "$BASE/api/sos" -d '{"lat":45.4,"lon":9.1}' | jget 'd["incident_id"]')
OPID=$(A "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("operatore@")))')
A -X POST "$BASE/api/incidents/$IID/assign" -d "{\"user_id\":\"$OPID\"}" | grep -q '"ok":true' && ok "incidente assegnato a operatore" || no "assign" "-"
A "$BASE/api/incidents" | python -c "import sys,json;d=json.load(sys.stdin);print(next((i.get('assignee') for i in d if i['id']=='$IID'),''))" | grep -q 'operatore@' && ok "assegnatario visibile in lista" || no "assignee list" "-"
O -X POST "$BASE/api/incidents/$IID/note" -d '{"body":"chiamato il lavoratore, risponde"}' | grep -q '"ok":true' && ok "nota operatore aggiunta" || no "note" "-"
A "$BASE/api/incidents/$IID/timeline" | grep -q 'chiamato il lavoratore' && ok "nota in timeline" || no "note timeline" "-"

# --- Check-in in centrale ---
M -X POST "$BASE/api/checkins" -d '{"minutes":30}' >/dev/null
O "$BASE/api/checkins/list" | grep -q 'mario@' && ok "check-in visibili in centrale" || no "list_checkins" "-"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
