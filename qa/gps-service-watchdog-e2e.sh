#!/usr/bin/env bash
# Watchdog del silenzio e stato di servizio dei badge (mig 0128), con orologio di test controllato.
# Ogni verifica è fatta poco PRIMA (riferimento + soglia - 5s) e poco DOPO (+5s) un intero
# silence_after_s dal riferimento (messa in servizio, passaggio di consegne o ultimo contatto).
#   1) badge già monitorato: allarme alla soglia, il monitoraggio continua dopo un nuovo contatto
#   2) badge in attesa di messa in servizio e badge fuori servizio: visibili, nessun incidente
#   3) badge appena messo in servizio che non ha MAI trasmesso: allarme alla soglia
#   4) cosa avvia il conteggio: l'assegnazione da sola NO, la messa in servizio SÌ; il passaggio di
#      consegne lo riavvia; senza lavoratore il badge torna in attesa
#   5) validazioni della messa in servizio
# Richiede core con TEST_CLOCK=1.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG=globex
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
prev(){ one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='$1'),'non_abilitato')"; }
setm(){ sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','$1','$2') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='$2'" >/dev/null; }
P_GPS=$(prev gps); P_LW=$(prev loneworker); setm gps attivo; setm loneworker attivo
P_CFG=$(sql "SELECT config::text FROM app.tenant_module_config WHERE tenant_id='$TID' AND module_code='gps' AND tool_code='alarm_map'")
sql "INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config) VALUES('$TID','gps','alarm_map','{\"silence_after_s\":600,\"silence_shift_only\":\"no\"}')
     ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config = app.tenant_module_config.config || '{\"silence_after_s\":600,\"silence_shift_only\":\"no\"}'::jsonb" >/dev/null
J=$(mktemp); curl -s -o /dev/null -c "$J" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$J"|tail -1)
A(){ curl -s -b "$J" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" -H 'content-type: application/json' "$@"; }
code(){ curl -s -o /dev/null -w '%{http_code}' -b "$J" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" -H 'content-type: application/json' "$@"; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP  richiede TEST_CLOCK=1"; exit 0; }
U1=$(one "SELECT id FROM app.users WHERE tenant_id='$TID' AND role='tenant_admin' LIMIT 1")
U2=$(one "SELECT id FROM app.users WHERE tenant_id='$TID' AND id<>'$U1' ORDER BY created_at LIMIT 1")

R=$RANDOM$RANDOM
mk(){ one "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user,enabled) VALUES('$TID','$1','$2','gps',$3,${4:-true}) RETURNING id"; }
WD(){ curl -s -o /dev/null -X POST "$BASE/api/_test/gps-silence-watchdog"; }
NOWC(){ curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | jv "d['now']"; }
# porta l'orologio a (riferimento del device + offset secondi). Il riferimento è quello del watchdog:
# GREATEST(ultimo contatto, armamento).
goto(){ local ref secs
  ref=$(one "SELECT to_char(GREATEST(last_seen_at, silence_armed_at) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM app.devices WHERE id='$1'")
  secs=$(python -c "import datetime as D,sys,math;p=lambda s:D.datetime.fromisoformat(s.replace('Z','+00:00'));print(max(0,math.ceil((p('$ref')-p('$(NOWC)')).total_seconds()+600+$2)))")
  curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$secs}"; WD; }
NI(){ one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$1' AND i.etype='perdita_contatto'"; }
svc(){ code -X POST "$BASE/api/devices/$1/service" -d "{\"state\":\"$2\"}"; }
state(){ one "SELECT service_state FROM app.devices WHERE id='$1'"; }

# ============ 1) badge già monitorato ===========================================================
D1=$(mk "svc1-$R" "monitorato" "'$U1'")
TOK="e2e-svc-$R"; sql "INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id) VALUES('$TOK',NULL,'$TID','gps','$D1')" >/dev/null
[ "$(svc "$D1" in_servizio)" = "200" ] && [ "$(state "$D1")" = "in_servizio" ] && ok "1) badge messo in servizio" || no "1) messa in servizio"
curl -s -o /dev/null "$BASE/api/ingress/osmand?token=$TOK&lat=45.1&lon=9.1"
goto "$D1" -5; [ "$(NI "$D1")" = "0" ] && ok "1) 5s prima della soglia dall'ultimo contatto: nessun allarme" || no "1) prima" "$(NI "$D1")"
goto "$D1" 5;  [ "$(NI "$D1")" = "1" ] && ok "1) 5s dopo la soglia: UN allarme perdita_contatto" || no "1) dopo" "$(NI "$D1")"
curl -s -o /dev/null "$BASE/api/ingress/osmand?token=$TOK&batt=80"     # il badge torna a trasmettere
goto "$D1" -5; [ "$(NI "$D1")" = "1" ] && ok "1) dopo un nuovo contatto il monitoraggio continua: nessun nuovo allarme prima della soglia" || no "1) continuità prima" "$(NI "$D1")"
goto "$D1" 5;  [ "$(NI "$D1")" = "2" ] && ok "1) nuovo silenzio oltre la soglia: secondo episodio" || no "1) continuità dopo" "$(NI "$D1")"

# ============ 2) in attesa / fuori servizio: visibili, nessun incidente =========================
D2=$(mk "svc2-$R" "in attesa" "'$U1'")
sql "UPDATE app.devices SET last_seen_at=now()-interval '3 hours' WHERE id='$D2'" >/dev/null
D2B=$(mk "svc2b-$R" "fuori servizio" "'$U2'")
svc "$D2B" in_servizio >/dev/null; svc "$D2B" fuori_servizio >/dev/null
[ "$(state "$D2")" = "in_attesa" ] && [ "$(state "$D2B")" = "fuori_servizio" ] && ok "2) stati: assegnato non in servizio = in_attesa; ritirato = fuori_servizio" || no "2) stati" "$(state "$D2")/$(state "$D2B")"
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":1300}'; WD
[ "$(NI "$D2")" = "0" ] && [ "$(NI "$D2B")" = "0" ] && ok "2) oltre due soglie: nessun incidente per badge in attesa o fuori servizio" || no "2) incidenti" "$(NI "$D2")/$(NI "$D2B")"
VIS=$(A "$BASE/api/devices" | jv "sorted(x['service_state'] for x in d if x['id'] in ('$D2','$D2B'))")
[ "$VIS" = "['fuori_servizio', 'in_attesa']" ] && ok "2) entrambi visibili nella lista dispositivi con il loro stato" || no "2) visibilità" "$VIS"

# ============ 3) appena messo in servizio, mai trasmesso ========================================
D3=$(mk "svc3-$R" "nuovo" "'$U1'")
[ "$(one "SELECT last_seen_at IS NULL FROM app.devices WHERE id='$D3'")" = "t" ] && ok "3) badge nuovo: nessun contatto registrato" || no "3) setup"
svc "$D3" in_servizio >/dev/null
goto "$D3" -5; [ "$(NI "$D3")" = "0" ] && ok "3) 5s prima della soglia dalla messa in servizio: nessun allarme" || no "3) prima" "$(NI "$D3")"
goto "$D3" 5;  [ "$(NI "$D3")" = "1" ] && ok "3) 5s dopo: allarme anche senza primo contatto" || no "3) dopo" "$(NI "$D3")"
NS=$(one "SELECT e.payload_ref->>'never_seen' FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$D3' AND i.etype='perdita_contatto'")
[ "$NS" = "true" ] && ok "3) l'allarme dichiara che il badge non si è mai collegato" || no "3) never_seen" "$NS"

# ============ 4) quale azione avvia il conteggio ================================================
D4=$(mk "svc4-$R" "solo assegnato" "NULL")
[ "$(code -X POST "$BASE/api/devices/$D4/assign" -d "{\"user_id\":\"$U2\"}")" = "200" ] && [ "$(state "$D4")" = "in_attesa" ] \
  && ok "4) l'assegnazione da sola NON mette in servizio" || no "4) assegnazione" "$(state "$D4")"
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":700}'; WD
[ "$(NI "$D4")" = "0" ] && ok "4) badge solo assegnato: nessun conteggio, nessun allarme" || no "4) conteggio" "$(NI "$D4")"
ARM0=$(one "SELECT to_char(silence_armed_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM app.devices WHERE id='$D3'")
[ "$(code -X POST "$BASE/api/devices/$D3/assign" -d "{\"user_id\":\"$U2\"}")" = "200" ] && [ "$(one "SELECT silence_armed_at > '$ARM0'::timestamptz FROM app.devices WHERE id='$D3'")" = "t" ] \
  && ok "4) passaggio di consegne di un badge in servizio: conteggio riavviato" || no "4) passaggio di consegne"
goto "$D3" -5; [ "$(NI "$D3")" = "1" ] && ok "4) nuovo portatore: nessun nuovo allarme prima della soglia" || no "4) consegna prima" "$(NI "$D3")"
goto "$D3" 5;  [ "$(NI "$D3")" = "2" ] && ok "4) nuovo portatore: allarme alla soglia" || no "4) consegna dopo" "$(NI "$D3")"
code -X POST "$BASE/api/devices/$D1/assign" -d '{"user_id":null}' >/dev/null
[ "$(state "$D1")" = "in_attesa" ] && ok "4) badge senza lavoratore: torna in attesa di messa in servizio" || no "4) senza lavoratore" "$(state "$D1")"
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":1300}'; WD
[ "$(NI "$D1")" = "2" ] && ok "4) badge non più assegnato: nessun nuovo allarme" || no "4) dopo rimozione" "$(NI "$D1")"

# ============ 5) validazioni ======================================================================
D5=$(mk "svc5-$R" "senza lavoratore" "NULL")
[ "$(svc "$D5" in_servizio)" = "400" ] && ok "5) in servizio senza lavoratore assegnato -> 400" || no "5) senza lavoratore"
D6=$(mk "svc6-$R" "disabilitato" "'$U1'" false)
[ "$(svc "$D6" in_servizio)" = "400" ] && ok "5) in servizio di un badge disabilitato -> 400" || no "5) disabilitato"
DI=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TID','svci-$R','sensore','iot') RETURNING id")
[ "$(svc "$DI" in_servizio)" = "400" ] && ok "5) solo dispositivi GPS/badge -> 400 per un sensore" || no "5) non gps"
[ "$(svc "$D5" boh)" = "400" ] && ok "5) stato sconosciuto -> 400" || no "5) stato"
sql "UPDATE app.devices SET enabled=false WHERE id='$D3'" >/dev/null
[ "$(state "$D3")" = "fuori_servizio" ] && ok "5) disabilitare un badge in servizio lo mette fuori servizio" || no "5) auto fuori servizio" "$(state "$D3")"

# ============ teardown ==========================================================================
for D in "$D1" "$D2" "$D2B" "$D3" "$D4" "$D5" "$D6" "$DI"; do
  sql "DELETE FROM app.incidents i USING app.events e WHERE i.event_id=e.id AND e.device_id='$D'" >/dev/null 2>&1
  sql "DELETE FROM app.devices WHERE id='$D'" >/dev/null 2>&1
done
sql "DELETE FROM app.ingest_tokens WHERE token='$TOK'" >/dev/null
if [ -n "$P_CFG" ]; then sql "UPDATE app.tenant_module_config SET config='$P_CFG'::jsonb WHERE tenant_id='$TID' AND module_code='gps' AND tool_code='alarm_map'" >/dev/null
else sql "DELETE FROM app.tenant_module_config WHERE tenant_id='$TID' AND module_code='gps' AND tool_code='alarm_map'" >/dev/null; fi
setm gps "$P_GPS"; setm loneworker "$P_LW"
[ "$(one "SELECT count(*) FROM app.devices WHERE external_id LIKE 'svc%-$R'")" = "0" ] && ok "teardown completo" || no "teardown"
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
