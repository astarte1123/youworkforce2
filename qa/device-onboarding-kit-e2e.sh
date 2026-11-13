#!/usr/bin/env bash
# Device Onboarding Kit (DOK): da un modello a catalogo genera device + credenziale + routing +
# profilo, differenziando per tipologia d'uso (category). + readiness + test-SOS sul percorso reale.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
GID=$(api "$BASE/api/groups" | jq "d['groups'][0]['id']")
WUID=$(api "$BASE/api/users" | jq "[u['id'] for u in d if u['role'] in ('operatore','tenant_admin')][0]")

# --- Personale (Teltonika TMT250): etype sos/man_down/immobilita/batteria + profilo ---
K1=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices/onboarding-kit" -d "{\"model_code\":\"teltonika-tmt250\",\"external_id\":\"IMEI-DOK-$RANDOM\",\"user_id\":\"$WUID\",\"group_id\":\"$GID\",\"make_profile\":true}")
CAT=$(echo "$K1" | jq "d['category']"); ET=$(echo "$K1" | jq "','.join(sorted(d['etypes']))"); PROF=$(echo "$K1" | jq "d.get('profile_id')"); TOK=$(echo "$K1" | jq "d['token'][:8]")
D1=$(echo "$K1" | jq "d['device_id']")
[ "$CAT" = "personale" ] && ok "personale: categoria corretta" || no "categoria" "$CAT"
echo "$ET" | grep -q "man_down" && echo "$ET" | grep -q "sos" && ok "personale: etype sos+man_down dedotti" || no "etype" "$ET"
[ "$PROF" != "None" ] && [ -n "$PROF" ] && ok "personale: profilo di protezione creato" || no "profilo" "$PROF"
[ -n "$TOK" ] && ok "personale: credenziale di ingest generata" || no "token" "-"

# --- Veicolo (Teltonika FMB920): nessun sos/man_down, nessun profilo ---
K2=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices/onboarding-kit" -d "{\"model_code\":\"teltonika-fmb920\",\"external_id\":\"IMEI-V-$RANDOM\",\"group_id\":\"$GID\"}")
CAT2=$(echo "$K2" | jq "d['category']"); PROF2=$(echo "$K2" | jq "d.get('profile_id')"); ET2=$(echo "$K2" | jq "','.join(d['etypes'])")
{ [ "$CAT2" = "veicolo" ] && [ "$PROF2" = "None" ]; } && ok "veicolo: categoria diversa, nessun profilo" || no "veicolo" "$CAT2/$PROF2"
echo "$ET2" | grep -q "sos" && no "veicolo non dovrebbe avere sos" "$ET2" || ok "veicolo: nessun segnale SOS (corretto)"

# --- Readiness: segnali dichiarati, sos NON ancora verificato ---
R1=$(api "$BASE/api/devices/$D1/readiness")
echo "$R1" | jq "d['states']['registrazione']" | grep -qi true && ok "readiness: stati presenti" || no "readiness stati" "$R1"
SOSV=$(echo "$R1" | jq "[s['verified'] for s in d['signals'] if s['etype']=='sos'][0]")
[ "$SOSV" = "False" ] && ok "readiness: sos non ancora verificato" || no "sos pre-test" "$SOSV"

# --- Test-SOS: percorso reale, poi sos risulta verificato ---
TS=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices/$D1/test-sos" | jq "d['ok']")
[ "$TS" = "True" ] && ok "test-SOS accettato (percorso reale)" || no "test-sos" "$TS"
sleep 1
SOSV2=$(api "$BASE/api/devices/$D1/readiness" | jq "[s['verified'] for s in d['signals'] if s['etype']=='sos'][0]")
[ "$SOSV2" = "True" ] && ok "readiness: dopo test-SOS il segnale sos risulta VERIFICATO" || no "sos post-test" "$SOSV2"

# --- Bulk: 3 righe (2 ok, 1 errore worker isolato) ---
GN=$(api "$BASE/api/groups" | jq "d['groups'][0]['name']")
RB=$RANDOM
BB=$(python -c "import json;print(json.dumps({'default_model_code':'teltonika-tmt250','default_group':'$GN','make_profile':True,'rows':[{'external_id':'BULKE-$RB-1'},{'external_id':'BULKE-$RB-2'},{'external_id':'BULKE-$RB-3','worker_email':'nope@x.y'}]}))")
RES=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices/onboarding-kit/bulk" -d "$BB")
CR=$(echo "$RES" | jq "d['created']"); FL=$(echo "$RES" | jq "d['failed']")
{ [ "$CR" = "2" ] && [ "$FL" = "1" ]; } && ok "bulk: 2 creati, 1 fallito (errore isolato)" || no "bulk ko" "created=$CR failed=$FL"
echo "$RES" | jq "[r for r in d['results'] if r['ok']][0].get('token') is not None" | grep -qi true && ok "bulk: token per-device nel risultato" || no "bulk token" "-"

# teardown
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" >/dev/null; }
sql "DELETE FROM app.ingest_tokens WHERE device_id IN (SELECT id FROM app.devices WHERE external_id LIKE 'BULKE-$RB-%')"
sql "DELETE FROM app.device_assignments WHERE device_id IN (SELECT id FROM app.devices WHERE external_id LIKE 'BULKE-$RB-%')"
sql "DELETE FROM app.devices WHERE external_id LIKE 'BULKE-$RB-%'"
sql "DELETE FROM app.incidents WHERE event_id IN (SELECT id FROM app.events WHERE device_id IN ('$D1'))"
sql "DELETE FROM app.events WHERE device_id IN ('$D1')"
sql "DELETE FROM app.device_assignments WHERE device_id IN ('$D1')"
sql "DELETE FROM app.ingest_tokens WHERE device_id IN ('$D1')"
sql "DELETE FROM app.protection_profile_members WHERE profile_id='$PROF'" 2>/dev/null || true
sql "DELETE FROM app.protection_profiles WHERE id='$PROF'" 2>/dev/null || true
sql "DELETE FROM app.devices WHERE model_code IN ('teltonika-tmt250','teltonika-fmb920') AND external_id LIKE 'IMEI-%'"
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
