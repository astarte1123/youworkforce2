#!/usr/bin/env bash
# Verifica di UPGRADE della migrazione 0128 partendo da un DB con 0127 già applicata, con orologio di
# test controllato (TEST_CLOCK=1). Non fa parte di run-all.sh (serve un dump a versione 127).
#   - classificazione dei badge esistenti: chi era monitorato resta in servizio CON l'armamento di
#     0127 invariato; assegnato mai collegato -> in attesa; disabilitato -> fuori servizio
#   - watchdog prima/dopo un intero silence_after_s dall'upgrade (riferimento conservato)
#   - titolarità GPS: tabella gps_identity popolata (contesi marcati), scritture concorrenti in
#     READ COMMITTED e REPEATABLE READ non creano duplicati tra tenant
# Uso: DUMP=/tmp/yw_pre_0128.dump BIN=core/target/debug/yw-core.exe bash qa/upgrade-0128-check.sh
set -uo pipefail
export MSYS_NO_PATHCONV=1
DUMP="${DUMP:-/tmp/yw_pre_0128.dump}"; BIN="${BIN:-core/target/debug/yw-core.exe}"
DB=yw_upgrade_0128; HP=18128; TP=15128; MP=11128; BASE="http://127.0.0.1:$HP"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d "$DB" -v ON_ERROR_STOP=1 -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

docker exec ywdb psql -U postgres -tAc "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1
docker exec ywdb psql -U postgres -tAc "CREATE DATABASE $DB" >/dev/null
docker exec ywdb psql -U postgres -tAc "ALTER DATABASE $DB SET search_path=app, public" >/dev/null
docker exec ywdb pg_restore -U postgres -d "$DB" --no-owner "$DUMP" 2>/dev/null
[ "$(one "SELECT max(version) FROM _sqlx_migrations")" = "127" ] && ok "DB di partenza a versione 127" || { no "versione" "$(one "SELECT max(version) FROM _sqlx_migrations")"; exit 1; }

ACME=$(one "SELECT id FROM app.tenants WHERE slug='acme'"); GLOBEX=$(one "SELECT id FROM app.tenants WHERE slug='globex'")
W=$(one "SELECT id FROM app.users WHERE tenant_id='$ACME' AND role='lavoratore' LIMIT 1")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$ACME','gps','attivo'),('$ACME','loneworker','attivo')
     ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo';
     UPDATE app.incidents SET state='chiuso', closed_at=now() WHERE tenant_id='$ACME' AND state<>'chiuso'" >/dev/null
sql "INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config) VALUES('$ACME','gps','alarm_map','{\"silence_after_s\":600}')
     ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config = app.tenant_module_config.config || '{\"silence_after_s\":600}'::jsonb" >/dev/null
# badge come li ha lasciati 0127
M1=$(one "INSERT INTO app.devices(tenant_id,external_id,source,label,assigned_user,enabled,last_seen_at,silence_armed_at)
          VALUES('$ACME','U128-MON','gps','monitorato','$W',true,now()-interval '1 minute',now()-interval '5 minutes') RETURNING id")
ARM_M1=$(one "SELECT to_char(silence_armed_at,'YYYY-MM-DD HH24:MI:SS.US') FROM app.devices WHERE id='$M1'")
M2=$(one "INSERT INTO app.devices(tenant_id,external_id,source,label,assigned_user,enabled) VALUES('$ACME','U128-NEW','gps','mai collegato','$W',true) RETURNING id")
M3=$(one "INSERT INTO app.devices(tenant_id,external_id,source,label,assigned_user,enabled,last_seen_at) VALUES('$ACME','U128-OFF','gps','disabilitato','$W',false,now()-interval '1 hour') RETURNING id")
M4=$(one "INSERT INTO app.devices(tenant_id,external_id,source,label,enabled,last_seen_at) VALUES('$ACME','U128-STOCK','gps','magazzino',true,now()-interval '1 hour') RETURNING id")
sql "ALTER TABLE app.devices DISABLE TRIGGER devices_gps_identity_unique;
     INSERT INTO app.devices(tenant_id,external_id,source,label) VALUES('$ACME','U128-DUP','gps','dup A'),('$GLOBEX','U128-DUP','gps','dup B');
     ALTER TABLE app.devices ENABLE TRIGGER devices_gps_identity_unique;" >/dev/null

LOG=$(mktemp)
YW_DB_OWNER="postgres://postgres:postgres@localhost:55432/$DB" YW_DB_RT="postgres://app_rt:app_rt_pw@localhost:55432/$DB" \
YW_SECRET_KEY="${YW_SECRET_KEY:-dev-secret-key-0123456789abcdef0123456789abcdef}" YW_BIND="127.0.0.1:$HP" YW_TELTONIKA_PORT=$TP YW_MQTT_PORT=$MP \
YW_ESC_SINK_URL="http://127.0.0.1:18099/" TEST_CLOCK=1 "$BIN" >"$LOG" 2>&1 &
CPID=$!
for i in $(seq 1 120); do [ "$(one "SELECT count(*) FROM _sqlx_migrations WHERE version=128" 2>/dev/null)" = "1" ] && break; sleep 0.25; done
[ "$(one "SELECT count(*) FROM _sqlx_migrations WHERE version=128")" = "1" ] && ok "0128 applicata all'avvio" || no "0128" "$(tail -3 "$LOG")"

st(){ one "SELECT service_state FROM app.devices WHERE id='$1'"; }
[ "$(st "$M1")" = "in_servizio" ] && [ "$(one "SELECT to_char(silence_armed_at,'YYYY-MM-DD HH24:MI:SS.US') FROM app.devices WHERE id='$M1'")" = "$ARM_M1" ] \
  && ok "badge monitorato: in servizio, armamento di 0127 conservato" || no "monitorato" "$(st "$M1")"
[ "$(st "$M2")" = "in_attesa" ] && ok "assegnato mai collegato: in attesa di messa in servizio (nessun allarme da upgrade)" || no "mai collegato" "$(st "$M2")"
[ "$(st "$M3")" = "fuori_servizio" ] && ok "disabilitato: fuori servizio" || no "disabilitato" "$(st "$M3")"
[ "$(st "$M4")" = "in_attesa" ] && ok "a magazzino (non assegnato): in attesa" || no "magazzino" "$(st "$M4")"
[ "$(one "SELECT contested::text||coalesce(tenant_id::text,'-') FROM app.gps_identity WHERE external_id='U128-DUP'")" = "true-" ] \
  && ok "titolarità: IMEI conteso marcato senza titolare" || no "conteso"
[ "$(one "SELECT tenant_id FROM app.gps_identity WHERE external_id='U128-MON'")" = "$ACME" ] && ok "titolarità: IMEI univoco col suo tenant" || no "titolare"

# IMEI contesi (sola lettura, con stato di servizio): duplicato elencato, nessun lavoratore monitorato
IMC=$(docker exec -i ywdb psql -U postgres -d "$DB" -tA -F '|' -v ON_ERROR_STOP=1 < "$(dirname "$0")/sql/check-imei-contesi.sql")
[ "$(echo "$IMC" | awk -F'|' '$1=="U128-DUP" && $9=="f"' | wc -l)" = "2" ] && echo "$IMC" | grep -q "^1|0$"   && ok "controllo IMEI contesi: 1 IMEI conteso, nessun lavoratore monitorato" || no "check imei" "$IMC"

# --- watchdog con orologio controllato, dall'upgrade -------------------------------------------
for i in $(seq 1 120); do curl -s -o /dev/null "$BASE/health" && break; sleep 0.5; done
WD(){ curl -s -o /dev/null -X POST "$BASE/api/_test/gps-silence-watchdog"; }
NOWC(){ curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | jv "d['now']"; }
goto(){ local ref secs
  ref=$(one "SELECT to_char(GREATEST(last_seen_at, silence_armed_at) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM app.devices WHERE id='$1'")
  secs=$(python -c "import datetime as D,math;p=lambda s:D.datetime.fromisoformat(s.replace('Z','+00:00'));print(max(0,math.ceil((p('$ref')-p('$(NOWC)')).total_seconds()+600+$2)))")
  curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$secs}"; WD; }
NI(){ one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$1' AND i.etype='perdita_contatto'"; }
goto "$M1" -5
[ "$(NI "$M1")" = "0" ] && ok "monitorato: 5s prima della soglia dall'ultimo contatto, nessun allarme" || no "M1 prima" "$(NI "$M1")"
goto "$M1" 5
[ "$(NI "$M1")" = "1" ] && ok "monitorato: 5s dopo la soglia, allarme (monitoraggio conservato)" || no "M1 dopo" "$(NI "$M1")"
[ "$(NI "$M2")$(NI "$M3")$(NI "$M4")" = "000" ] && ok "in attesa / fuori servizio / magazzino: nessun allarme dopo la soglia" || no "altri" "$(NI "$M2")$(NI "$M3")$(NI "$M4")"
# messa in servizio dopo l'upgrade del badge mai collegato
J=$(mktemp); LR=$(curl -s -c "$J" -X POST "$BASE/api/login" -H "content-type: application/json" -H "x-tenant-slug: acme" -d "{\"email\":\"admin@acme.loneworker.eu\",\"password\":\"demo\"}")
CS=$(echo "$LR" | jv "d['csrf']")
RS=$(curl -s -w " %{http_code}" -b "$J" -H "x-tenant-slug: acme" -H "x-csrf-token: $CS" -H "content-type: application/json" -X POST "$BASE/api/devices/$M2/service" -d "{\"state\":\"in_servizio\"}"); C=${RS##* }
[ "$C" = "200" ] && ok "badge mai collegato messo in servizio" || no "messa in servizio" "$RS"
goto "$M2" -5; [ "$(NI "$M2")" = "0" ] && ok "appena in servizio: nessun allarme prima della soglia" || no "M2 prima" "$(NI "$M2")"
goto "$M2" 5;  [ "$(NI "$M2")" = "1" ] && ok "appena in servizio, mai collegato: allarme alla soglia" || no "M2 dopo" "$(NI "$M2")"

# --- titolarità con scritture concorrenti --------------------------------------------------------
TURNI=$(one "SELECT id FROM app.tenants WHERE slug='turni'")
race(){ # $1 = isolamento della seconda transazione, $2 = IMEI
  docker exec -i ywdb psql -U postgres -d "$DB" -q >/dev/null 2>&1 <<SQL &
BEGIN; INSERT INTO app.devices(tenant_id,external_id,source) VALUES('$ACME','$2','gps'); SELECT pg_sleep(2); COMMIT;
SQL
  local p1=$!; sleep 0.7
  local out; out=$(docker exec -i ywdb psql -U postgres -d "$DB" 2>&1 <<SQL
BEGIN ISOLATION LEVEL $1; SELECT 1; INSERT INTO app.devices(tenant_id,external_id,source) VALUES('$TURNI','$2','gps'); COMMIT;
SQL
)
  wait $p1; echo "$out" | tr '\n' ' '
}
for ISO in "READ COMMITTED" "REPEATABLE READ"; do
  IM="RACE-${ISO// /}-$RANDOM"
  OUT=$(race "$ISO" "$IM")
  N=$(one "SELECT count(DISTINCT tenant_id) FROM app.devices WHERE external_id='$IM'")
  [ "$N" = "1" ] && echo "$OUT" | grep -qiE "già registrato|could not serialize" \
    && ok "scritture concorrenti ($ISO): un solo tenant titolare, la seconda è rifiutata" || no "race $ISO" "tenants=$N out=$OUT"
done
# rilascio: cancellato il titolare, l'IMEI diventa registrabile da un altro tenant
sql "DELETE FROM app.devices WHERE external_id='U128-MON'" >/dev/null
E=$(docker exec -i ywdb psql -U postgres -d "$DB" -tAc "INSERT INTO app.devices(tenant_id,external_id,source) VALUES('$GLOBEX','U128-MON','gps') RETURNING 'ok'" 2>&1)
echo "$E" | grep -q "ok" && [ "$(one "SELECT tenant_id FROM app.gps_identity WHERE external_id='U128-MON'")" = "$GLOBEX" ] \
  && ok "titolarità liberata alla cancellazione e riassegnata al nuovo tenant" || no "rilascio" "$E"
# conteso: risolto eliminando il doppione sbagliato
sql "DELETE FROM app.devices WHERE external_id='U128-DUP' AND tenant_id='$GLOBEX'" >/dev/null
[ "$(one "SELECT contested::text||tenant_id FROM app.gps_identity WHERE external_id='U128-DUP'")" = "false$ACME" ] \
  && ok "IMEI conteso risolto eliminando il doppione: titolare unico" || no "risoluzione conteso"

kill $CPID 2>/dev/null; wait $CPID 2>/dev/null
[ -n "${KEEP:-}" ] || docker exec ywdb psql -U postgres -tAc "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
