#!/usr/bin/env bash
# Percorso GPS/badge NATIVO end-to-end (nessun server esterno). Tenant globex (isolato).
#   1) i residui del vecchio connettore Traccar non esistono più (route, tabelle, colonne, modulo)
#   2) ingest JSON HTTP di posizioni -> storico -> report nativi (riepilogo, soste, tragitti, CSV)
#   3) policy eventi per tipo: un evento reso "critico" diventa incidente
#   4) comandi rapidi nativi (GPRS): catalogo + esito registrato anche se il device è scollegato
#   5) watchdog del silenzio (salva-vita): device assegnato e muto -> UN incidente perdita_contatto
# Richiede core con TEST_CLOCK=1 per il punto 5 (hook di test); altrimenti il punto è saltato.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"
PASS=0; FAIL=0; SKIP=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sk(){ echo "SKIP  $1  (${2:-})"; SKIP=$((SKIP+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
prev(){ one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='$1'),'non_abilitato')"; }
on(){ sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','$1','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null; }
P_GPS=$(prev gps); P_LW=$(prev loneworker); P_IOT=$(prev iot)
on loneworker; on gps; on iot

# --- 1) nessun residuo del connettore esterno ---------------------------------------------------
[ "$(code "$BASE/api/traccar/status")" = "404" ] && ok "route /api/traccar/* rimosse" || no "route traccar ancora presente"
[ "$(code -X POST "$BASE/api/ingress/traccar" -H 'content-type: application/json' -d '{}')" = "404" ] && ok "ingest /api/ingress/traccar rimosso" || no "ingest traccar presente"
[ "$(one "SELECT count(*) FROM app.modules_catalog WHERE code='traccar'")" = "0" ] && [ "$(one "SELECT count(*) FROM app.modules_catalog WHERE code='gps'")" = "1" ] \
  && ok "modulo 'gps' (Localizzazione GPS e badge) al posto di 'traccar'" || no "catalogo moduli"
[ "$(one "SELECT count(*) FROM information_schema.tables WHERE table_schema='app' AND table_name LIKE 'traccar%'")" = "0" ] \
  && [ "$(one "SELECT count(*) FROM information_schema.columns WHERE table_schema='app' AND column_name LIKE 'traccar%'")" = "0" ] \
  && ok "nessuna tabella/colonna traccar nel DB" || no "residui traccar nel DB"

# --- setup: lavoratore, device assegnato, token device-bound, sessione admin ----------------------
R=$RANDOM$RANDOM; IMEI="35$R$RANDOM"; TOK="e2e-gps-$R"
UID_W=$(one "SELECT id FROM app.users WHERE tenant_id='$TID' ORDER BY created_at LIMIT 1")
DID=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user) VALUES('$TID','$IMEI','GPS e2e $R','gps','$UID_W') RETURNING id")
sql "INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id) VALUES('$TOK',NULL,'$TID','gps','$DID')" >/dev/null
AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
AC=$(awk '$6=="yw_csrf"{print $7}' "$AJ" | tail -1)
A(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' "$@"; }
[ -n "$DID" ] && ok "device GPS assegnato creato" || { no "setup device"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

# --- 2) posizioni: fermo 10 min -> ~1.1 km in 5 min -> fermo 10 min (ieri) -----------------------
T0=$(python -c "import time;print(int(time.time())-86400)")
post_pos(){ curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/ingress/gps/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$IMEI\"},\"position\":{\"id\":\"$R-$1\",\"latitude\":$2,\"longitude\":9.0,\"fixTime\":\"$(python -c "import datetime;print(datetime.datetime.utcfromtimestamp($T0+$1*60).strftime('%Y-%m-%dT%H:%M:%SZ'))")\"}}"; }
BAD=0
for m in $(seq 0 10); do [ "$(post_pos $m 45.0)" = "200" ] || BAD=1; done
for i in 1 2 3 4 5; do [ "$(post_pos $((10+i)) "$(python -c "print(45.0+0.002*$i)")")" = "200" ] || BAD=1; done
for m in $(seq 16 26); do [ "$(post_pos $m 45.010)" = "200" ] || BAD=1; done
[ "$BAD" = "0" ] && ok "27 posizioni ingerite via JSON HTTP" || no "ingest posizioni"
STORED=$(one "SELECT count(*) FROM app.device_positions WHERE device_id='$DID'")
[ "${STORED:-0}" -ge 9 ] && ok "storico posizioni salvato ($STORED fix, duplicati da fermo filtrati)" || no "storico" "$STORED"
[ -n "$(one "SELECT last_seen_at FROM app.devices WHERE id='$DID'")" ] && ok "ultimo contatto registrato (last_seen_at)" || no "last_seen_at vuoto"

FROM=$(python -c "import datetime;print(datetime.datetime.utcfromtimestamp($T0-600).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TO=$(python -c "import datetime;print(datetime.datetime.utcfromtimestamp($T0+3600).strftime('%Y-%m-%dT%H:%M:%SZ'))")
SUM=$(A "$BASE/api/devices/$DID/report/summary?from=$FROM&to=$TO")
echo "$SUM" | jv "0 if d['trips']>=1 and d['stops']>=2 and 900<d['distance_m']<1400 else 1" | grep -q '^0$' \
  && ok "riepilogo nativo: tragitti/soste/distanza coerenti" || no "riepilogo" "$SUM"
STOPS=$(A "$BASE/api/devices/$DID/report/stops?from=$FROM&to=$TO" | jv "len(d)")
[ "$STOPS" = "2" ] && ok "report soste: 2 soste" || no "soste" "$STOPS"
TRIPS=$(A "$BASE/api/devices/$DID/report/trips?from=$FROM&to=$TO" | jv "len(d)")
[ "$TRIPS" = "1" ] && ok "report tragitti: 1 tragitto" || no "tragitti" "$TRIPS"
CT=$(curl -s -o /dev/null -w '%{content_type}' -b "$AJ" -H "x-tenant-slug: $SLUG" "$BASE/api/devices/$DID/report/trips?from=$FROM&to=$TO&csv=true")
echo "$CT" | grep -q 'text/csv' && ok "export CSV" || no "csv" "$CT"
[ "$(code -b "$AJ" -H "x-tenant-slug: $SLUG" "$BASE/api/devices/$DID/report/boh?from=$FROM&to=$TO")" = "400" ] && ok "report sconosciuto -> 400" || no "validazione kind"

# --- 3) policy eventi --------------------------------------------------------------------------
A "$BASE/api/gps/event-policy" | grep -q 'deviceOverspeed' && ok "policy eventi leggibile" || no "policy get"
A -X POST "$BASE/api/gps/event-policy" -d '{"events":{"deviceOverspeed":"critico"}}' | grep -q '"ok":true' && ok "policy: eccesso velocità = critico" || no "policy set"
C=$(code -X POST "$BASE/api/ingress/gps?token=$TOK" -H 'content-type: application/json' -d "{\"device\":{\"uniqueId\":\"$IMEI\"},\"event\":{\"id\":\"ov-$R\",\"type\":\"deviceOverspeed\"}}")
sleep 1
[ "$(one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID' AND i.etype='eccesso_velocita_critico'")" -ge 1 ] \
  && ok "evento critico -> incidente (HTTP $C)" || no "incidente da policy" "$C"
A -X POST "$BASE/api/gps/event-policy" -d '{"events":{"deviceOverspeed":"registra"}}' >/dev/null

# --- 4) comandi nativi -------------------------------------------------------------------------
A "$BASE/api/devices/$DID/command-types" | grep -q '"posizione"' && ok "catalogo comandi rapidi nativo" || no "command-types"
CR=$(A -X POST "$BASE/api/devices/$DID/command" -d '{"type":"posizione"}')
echo "$CR" | jv "0 if d['ok'] is False and d['status_code']==502 else 1" | grep -q '^0$' && ok "device scollegato: esito 'non inviato' chiaro" || no "comando" "$CR"
A "$BASE/api/devices/$DID/commands" | grep -q '"status_code":502' && ok "esito comando nello storico" || no "storico comandi"
[ "$(code -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/command" -d '{"type":"inesistente"}')" = "400" ] \
  && ok "comando sconosciuto -> 400" || no "validazione comando"

# --- 5) watchdog del silenzio ------------------------------------------------------------------
# silenzio "vecchio" sia come ultimo contatto sia come armamento (0127: il watchdog conta da
# GREATEST(last_seen_at, silence_armed_at), così un upgrade o una consegna non generano allarmi)
sql "UPDATE app.devices SET service_state='in_servizio', last_seen_at='2000-01-01T00:00:00Z', silence_armed_at='2000-01-01T00:00:00Z' WHERE id='$DID'" >/dev/null
WC=$(code -X POST "$BASE/api/_test/gps-silence-watchdog")
if [ "$WC" = "200" ]; then
  code -X POST "$BASE/api/_test/gps-silence-watchdog" >/dev/null
  N=$(one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID' AND i.etype='perdita_contatto'")
  [ "$N" = "1" ] && ok "device muto -> UN incidente perdita_contatto (nessun duplicato)" || no "watchdog silenzio" "n=$N"
  sql "UPDATE app.devices SET assigned_user=NULL, last_seen_at='2000-01-02T00:00:00Z', silence_armed_at='2000-01-02T00:00:00Z' WHERE id='$DID'" >/dev/null
  code -X POST "$BASE/api/_test/gps-silence-watchdog" >/dev/null
  N2=$(one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID' AND i.etype='perdita_contatto'")
  [ "$N2" = "1" ] && ok "device non assegnato: nessun allarme di silenzio" || no "watchdog su device non assegnato" "n=$N2"
else sk "watchdog silenzio" "hook di test non disponibile (TEST_CLOCK off, HTTP $WC)"; fi

# --- teardown ----------------------------------------------------------------------------------
sql "DELETE FROM app.incidents i USING app.events e WHERE i.event_id=e.id AND e.device_id='$DID'" >/dev/null 2>&1
sql "DELETE FROM app.events WHERE device_id='$DID'" >/dev/null 2>&1
sql "DELETE FROM app.devices WHERE id='$DID'" >/dev/null 2>&1
[ "$(one "SELECT count(*) FROM app.devices WHERE id='$DID'")" = "0" ] && ok "teardown completo" || no "teardown"
sql "UPDATE app.tenant_modules SET state='$P_GPS' WHERE tenant_id='$TID' AND module_code='gps'" >/dev/null
sql "UPDATE app.tenant_modules SET state='$P_LW' WHERE tenant_id='$TID' AND module_code='loneworker'" >/dev/null
sql "UPDATE app.tenant_modules SET state='$P_IOT' WHERE tenant_id='$TID' AND module_code='iot'" >/dev/null
echo "== $PASS passed, $FAIL failed, $SKIP skipped =="
[ "$FAIL" = 0 ]
