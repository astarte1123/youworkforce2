#!/usr/bin/env bash
# Perdita di contatto (Q4, mig 0137): legata al turno, si chiude da sola, vale anche per l'app.
#  - badge in servizio, lavoratore FUORI turno: nessun allarme; la scheda lo dice
#  - turno aperto nell'app: allarme alla soglia (5 s prima no, 5 s dopo sì)
#  - il badge torna in linea (handshake Teltonika): l'allarme si chiude da solo, con traccia
#  - turno solo pianificato (nessuna app): il badge è sorvegliato
#  - app del lavoratore senza badge: fuori turno non sorvegliata; in turno allarme alla soglia
#    dell'app; il battito successivo lo chiude
#  - chi ha un badge in servizio: l'app non è sorvegliata (niente doppi allarmi)
# Tenant usa-e-getta. Richiede TEST_CLOCK=1 e il bridge Teltonika (YW_TELTONIKA_PORT, default 5027).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; THOST="${THOST:-127.0.0.1}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
qq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
adv(){ [ -n "${ADVF:-}" ] && echo $(( $(cat "$ADVF") + $1 )) > "$ADVF"; curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}" | py "d.get('now','')" 2>/dev/null; }
NOW=$(adv 0); [ -n "$NOW" ] || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
WD(){ curl -s -o /dev/null -X POST "$BASE/api/_test/gps-silence-watchdog"; }
J=$(mktemp -d); ADVF="$J/adv"; echo 0 > "$ADVF"
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ss$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ss $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"gps\",\"loneworker\"]}" | py "d.get('id','')")
# l'orologio di test torna indietro a fine prova (adv gira anche in subshell: totale su file)
trap 'curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H "content-type: application/json" -d "{\"seconds\":-$(cat "$ADVF" 2>/dev/null || echo 0)}"; curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
U1=$(mk w1); U2=$(mk w2); U3=$(mk w3)
login W1 "w1@$SLUG.test" WorkPass9; login W3 "w3@$SLUG.test" WorkPass9
IMEI1=$(python -c "import random;print('351'+str(random.randint(10**11,10**12-1)))")
IMEI2=$(python -c "import random;print('352'+str(random.randint(10**11,10**12-1)))")
badge(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user) VALUES('$TID','$1','$2','gps','$3') RETURNING id" | head -1 | tr -d '[:space:]'; }
D1=$(badge "$IMEI1" "Badge w1" "$U1"); D2=$(badge "$IMEI2" "Badge w2" "$U2")
call A POST "/api/devices/$D1/service" '{"state":"in_servizio"}' >/dev/null
call A POST "/api/devices/$D2/service" '{"state":"in_servizio"}' >/dev/null
# riferimento comune = orologio di test attuale (l'inizio turno è scritto col clock del DB)
ref(){ NOW=$(adv 0); qq "UPDATE app.devices SET last_seen_at='$NOW', silence_armed_at='$NOW' WHERE id='$1'"; }
NI(){ q "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.tenant_id='$TID' AND e.device_id='$1' AND i.etype='perdita_contatto'${2:+ AND i.state$2}"; }
export TPORT THOST
hs(){ IMEI="$1" python - <<'PY'
import socket,struct,os
imei=os.environ["IMEI"].encode()
s=socket.create_connection((os.environ["THOST"],int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei); r=s.recv(1); print(r.hex() if r else "none"); s.close()
PY
}

echo "== badge, lavoratore fuori turno =="
ref "$D1"; adv 1300 >/dev/null; WD
[ "$(NI "$D1")" = "0" ] && ok "fuori turno: oltre due soglie nessun allarme" || no "fuori turno" "$(NI "$D1")"
call A GET /api/devices | py "[(x['in_shift'],x['silence_shift_only']) for x in d if x['id']=='$D1'][0]" | grep -q "(False, True)" \
  && ok "la scheda dice che il lavoratore non è in turno (silenzio non sorvegliato)" || no "scheda" "-"

echo "== turno aperto nell'app =="
call W1 POST /api/shifts/start '{}' >/dev/null
ref "$D1"; qq "UPDATE app.shifts SET started_at='$NOW' WHERE tenant_id='$TID' AND user_id='$U1' AND state='in_corso'"
adv 595 >/dev/null; WD; [ "$(NI "$D1")" = "0" ] && ok "in turno: 5 s prima della soglia nessun allarme" || no "prima" "$(NI "$D1")"
adv 10 >/dev/null; WD;  [ "$(NI "$D1" "='aperto'")" = "1" ] && ok "in turno: 5 s dopo la soglia allarme Perdita di contatto" || no "dopo" "$(NI "$D1")"
call A GET /api/devices | py "[x['in_shift'] for x in d if x['id']=='$D1'][0]" | grep -q True && ok "la scheda dice in turno" || no "scheda in turno" "-"

echo "== il badge torna in linea =="
[ "$(hs "$IMEI1")" = "01" ] && ok "il badge si ricollega" || no "handshake" "-"
sleep 1
[ "$(NI "$D1" "='chiuso'")" = "1" ] && ok "l'allarme si chiude da solo" || no "auto-close" "$(NI "$D1" "='chiuso'")"
q "SELECT i.outcome FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$D1' AND i.etype='perdita_contatto'" | grep -q "rientrato" \
  && ok "esito: rientrato, dispositivo di nuovo in linea" || no "esito" "-"
[ "$(q "SELECT count(*) FROM app.incident_timeline t JOIN app.incidents i ON i.id=t.incident_id JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$D1' AND t.detail->>'by'='sistema' AND t.detail->>'auto'='true'")" = "1" ] \
  && ok "traccia in timeline (chiuso dal sistema)" || no "timeline" "-"

echo "== turno solo pianificato =="
ref "$D2"
qq "INSERT INTO app.shift_plans(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$U2','$NOW'::timestamptz - interval '1 hour','$NOW'::timestamptz + interval '8 hours')"
adv 605 >/dev/null; WD
[ "$(NI "$D2")" = "1" ] && ok "turno pianificato in corso: badge sorvegliato, allarme alla soglia" || no "pianificato" "$(NI "$D2")"

echo "== app del lavoratore senza badge =="
H=$(call W3 POST /api/worker/heartbeat '{}')
echo "$H" | py "d['monitored']" | grep -q False && ok "app fuori turno: non sorvegliata" || no "hb off" "$H"
DP=$(q "SELECT id FROM app.devices WHERE tenant_id='$TID' AND source='pwa' AND assigned_user='$U3'")
adv 2000 >/dev/null; WD
[ "$(NI "$DP")" = "0" ] && ok "app fuori turno silenziosa: nessun allarme" || no "app fuori turno" "$(NI "$DP")"
call W3 POST /api/shifts/start '{}' >/dev/null
H=$(call W3 POST /api/worker/heartbeat '{}')
echo "$H" | py "d['monitored'] and d['silence_after_s']==900" | grep -q True && ok "app in turno: sorvegliata (soglia 15 min)" || no "hb on" "$H"
NOW=$(adv 0); qq "UPDATE app.shifts SET started_at='$NOW' WHERE tenant_id='$TID' AND user_id='$U3' AND state='in_corso'"
qq "UPDATE app.devices SET last_seen_at='$NOW', last_pos_at=NULL WHERE id='$DP'"
adv 895 >/dev/null; WD; [ "$(NI "$DP")" = "0" ] && ok "app in turno: 5 s prima della soglia nessun allarme" || no "app prima" "$(NI "$DP")"
adv 10 >/dev/null; WD;  [ "$(NI "$DP" "='aperto'")" = "1" ] && ok "app in turno silenziosa: allarme Perdita di contatto" || no "app dopo" "$(NI "$DP")"
call W3 POST /api/worker/heartbeat '{}' >/dev/null
[ "$(NI "$DP" "='chiuso'")" = "1" ] && ok "l'app torna in linea: allarme chiuso da solo" || no "app auto-close" "$(NI "$DP" "='chiuso'")"

echo "== app di chi ha un badge in servizio =="
H=$(call W1 POST /api/worker/heartbeat '{}')
echo "$H" | py "(not d['monitored']) and d['badge_in_service']" | grep -q True && ok "badge in servizio: l'app non è sorvegliata (niente doppioni)" || no "badge+app" "$H"

echo "silence-shift: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
