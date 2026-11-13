#!/usr/bin/env bash
# Ingest NATIVO OsmAnd/HTTP (senza server Traccar): posizione + allarme -> incidente, con
# token device-bound (anti-spoof) e attribuzione temporale (timestamp del device).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
GID=$(api "$BASE/api/groups" | jq "d['groups'][0]['id']")
WUID=$(api "$BASE/api/users" | jq "[u['id'] for u in d if u['role'] in ('operatore','tenant_admin')][0]")
R=$RANDOM
KIT=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices/onboarding-kit" -d "{\"model_code\":\"teltonika-tmt250\",\"external_id\":\"OSME-$R\",\"user_id\":\"$WUID\",\"group_id\":\"$GID\"}")
TOK=$(echo "$KIT" | jq "d['token']"); DEV=$(echo "$KIT" | jq "d['device_id']")
[ -n "$TOK" ] && ok "kit creato (device + token)" || no "kit ko" "-"

# 1) posizione: aggiorna ultima posizione del device
P=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/ingress/osmand?token=$TOK&lat=45.4600&lon=9.1900&batt=88&timestamp=$(python -c 'import time;print(int(time.time()))')")
[ "$P" = 200 ] && ok "posizione OsmAnd accettata (200)" || no "pos ko" "$P"
sleep 1
LP=$(sql "SELECT last_lat||','||last_battery FROM app.devices WHERE id='$DEV'")
echo "$LP" | grep -q "45.46" && echo "$LP" | grep -q "88" && ok "ultima posizione + batteria aggiornate" || no "lastpos ko" "$LP"

# 2) token NON valido -> 401 (sicurezza)
U=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/ingress/osmand?token=tok-falso&lat=45&lon=9")
[ "$U" = 401 ] && ok "token non valido rifiutato (401)" || no "auth ko" "$U"

# 3) allarme SOS -> incidente sos attribuito al lavoratore (certo, orario affidabile)
TS=$(python -c 'import time;print(int(time.time()))')
A=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/ingress/osmand?token=$TOK&lat=45.47&lon=9.20&alarm=sos&batt=80&timestamp=$TS")
[ "$A" = 200 ] && ok "allarme SOS OsmAnd accettato (200)" || no "sos ko" "$A"
sleep 1
INC=$(sql "SELECT i.etype||'|'||COALESCE(u.email,'-')||'|'||i.attribution_uncertain FROM app.incidents i LEFT JOIN app.users u ON u.id=i.worker_user_id JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DEV' ORDER BY i.opened_at DESC LIMIT 1")
echo "$INC" | grep -q "^sos|" && ok "incidente SOS creato dal canale OsmAnd" || no "incidente ko" "$INC"
{ echo "$INC" | grep -qE "@" && echo "$INC" | grep -qiE "\|(f|false)$"; } && ok "lavoratore attribuito (attribuzione certa)" || no "attribuzione ko" "$INC"

# teardown
sql "DELETE FROM app.incidents WHERE event_id IN (SELECT id FROM app.events WHERE device_id='$DEV')" >/dev/null
sql "DELETE FROM app.events WHERE device_id='$DEV'" >/dev/null
sql "DELETE FROM app.device_assignments WHERE device_id='$DEV'" >/dev/null
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DEV'" >/dev/null
sql "DELETE FROM app.protection_profile_members m USING app.protection_profiles p WHERE m.profile_id=p.id AND p.name LIKE '%OSME-$R%'" >/dev/null
sql "DELETE FROM app.protection_profiles WHERE name LIKE '%OSME-$R%'" >/dev/null
sql "DELETE FROM app.devices WHERE id='$DEV'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
