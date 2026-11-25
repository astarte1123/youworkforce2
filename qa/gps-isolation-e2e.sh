#!/usr/bin/env bash
# Isolamento tenant e affidabilità del percorso GPS/badge nativo (patch dopo review di b904493).
#   1) bridge Teltonika simulato: identità IMEI univoca, dati/SOS nel tenant giusto, comandi da TUTTI
#      i percorsi di invio (comandi rapidi, GPRS grezzo, RPC IoT) solo verso il socket del proprio
#      tenant; IMEI conteso tra tenant rifiutato; testo libero = amministrazione dispositivi
#   2) ingest JSON con loneworker+gps e iot SPENTO: posizioni e SOS accettati
#   3) OsmAnd senza coordinate aggiorna la presenza; il watchdog misura l'ultimo CONTATTO, non
#      l'ultima posizione (età posizione ≠ età heartbeat); armamento all'assegnazione
#   4) configurazione strumento: campo omesso -> default, chiavi gestite altrove preservate,
#      risposta = valore salvato; scheda dispositivo con la soglia reale
#   5) report: oltre il limite di punti il risultato è dichiarato parziale (JSON, header, CSV)
# Richiede core con TEST_CLOCK=1 (hook di test) e bridge Teltonika su TPORT.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

TA=$(one "SELECT id FROM app.tenants WHERE slug='globex'"); TB=$(one "SELECT id FROM app.tenants WHERE slug='acme'")
prev(){ one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$1' AND module_code='$2'),'non_abilitato')"; }
setm(){ sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$1','$2','$3') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='$3'" >/dev/null; }
PA_GPS=$(prev $TA gps); PA_LW=$(prev $TA loneworker); PA_IOT=$(prev $TA iot); PB_GPS=$(prev $TB gps); PB_IOT=$(prev $TB iot)
PA_CFG=$(sql "SELECT config::text FROM app.tenant_module_config WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'")
setm $TA loneworker attivo; setm $TA gps attivo; setm $TA iot non_abilitato; setm $TB gps attivo; setm $TB iot attivo

login(){ local j; j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $1" -d "{\"email\":\"$2\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login globex admin@globex.loneworker.eu); AC=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
BJ=$(login acme admin@acme.loneworker.eu);     BC=$(awk '$6=="yw_csrf"{print $7}' "$BJ"|tail -1)
OJ=$(login acme operatore@acme.loneworker.eu); OC=$(awk '$6=="yw_csrf"{print $7}' "$OJ"|tail -1)
A(){ curl -s -b "$AJ" -H "x-tenant-slug: globex" -H "x-csrf-token: $AC" -H 'content-type: application/json' "$@"; }
B(){ curl -s -b "$BJ" -H "x-tenant-slug: acme" -H "x-csrf-token: $BC" -H 'content-type: application/json' "$@"; }
O(){ curl -s -b "$OJ" -H "x-tenant-slug: acme" -H "x-csrf-token: $OC" -H 'content-type: application/json' "$@"; }

R=$RANDOM$RANDOM; IMEI=$(python -c "import random;print('357'+str(random.randint(10**11,10**12-1)))")
UA=$(one "SELECT id FROM app.users WHERE tenant_id='$TA' ORDER BY created_at LIMIT 1")
DA=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user) VALUES('$TA','$IMEI','badge A $R','gps','$UA') RETURNING id")
[ -n "$DA" ] && ok "device GPS del tenant A censito" || { no "setup"; exit 1; }

# ============ 1) identità + isolamento Teltonika =================================================
C=$(B -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices" -d "{\"external_id\":\"$IMEI\",\"source\":\"gps\",\"label\":\"furto\"}")
[ "$C" = "409" ] && ok "tenant B non può registrare l'IMEI del tenant A (409)" || no "duplicato via API" "$C"

READY=$(mktemp); GOT=$(mktemp); : > "$GOT"
TPORT=$TPORT IMEI=$IMEI READY=$READY GOT=$GOT python - <<'PY' &
import socket,struct,os,time
imei=os.environ["IMEI"].encode()
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=30)
s.sendall(struct.pack(">H",len(imei))+imei); assert s.recv(1)==b"\x01"
# SOS: Codec8 Extended, event IO 1 (mappa di default -> sos)
ts=int(time.time()*1000); rec=struct.pack(">Q",ts)+b"\x01"+struct.pack(">ii",int(9.19*1e7),int(45.47*1e7))+struct.pack(">HHBH",0,0,7,0)
rec+=struct.pack(">H",1)+struct.pack(">H",0)+struct.pack(">H",0)*4+struct.pack(">H",0)
data=b"\x8e\x01"+rec+b"\x01"; s.sendall(b"\x00"*4+struct.pack(">I",len(data))+data+b"\x00"*4)
assert struct.unpack(">I",s.recv(4))[0]==1
open(os.environ["READY"],"w").write("1")
def crc16(d):
  c=0
  for b in d:
    c^=b
    for _ in range(8): c=(c>>1)^0xA001 if c&1 else c>>1
  return c
end=time.time()+25
s.settimeout(1)
while time.time()<end:
  try: h=s.recv(8)
  except socket.timeout: continue
  if len(h)<8: break
  dl=struct.unpack(">I",h[4:8])[0]; body=b""
  while len(body)<dl+4: body+=s.recv(dl+4-len(body))
  d=body[:dl]
  if d[0]==0x0C and d[2]==0x05:
    size=struct.unpack(">I",d[3:7])[0]; cmd=d[7:7+size].decode()
    open(os.environ["GOT"],"a").write(cmd+"\n")
    resp=("OK:"+cmd).encode(); inner=bytes([0x0C,1,6])+struct.pack(">I",len(resp))+resp+b"\x01"
    s.sendall(b"\x00"*4+struct.pack(">I",len(inner))+inner+struct.pack(">I",crc16(inner)))
s.close()
PY
SIM=$!
for i in $(seq 1 50); do [ -s "$READY" ] && break; sleep 0.2; done
[ -s "$READY" ] && ok "badge simulato collegato al bridge (accettato per il tenant A)" || no "connessione simulata" "-"
sleep 1
NA=$(one "SELECT count(*) FROM app.incidents WHERE tenant_id='$TA' AND correlation LIKE 'telt-$IMEI-%' AND etype='sos'")
NB=$(one "SELECT count(*) FROM app.incidents WHERE tenant_id='$TB' AND correlation LIKE 'telt-$IMEI-%'")
[ "$NA" = "1" ] && [ "$NB" = "0" ] && ok "SOS dal badge -> incidente SOLO nel tenant A" || no "instradamento SOS" "A=$NA B=$NB"

# duplicato "storico" nel tenant B (inserito aggirando il vincolo, come un dato precedente a 0127)
sql "ALTER TABLE app.devices DISABLE TRIGGER devices_gps_identity_unique" >/dev/null
DB=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TB','$IMEI','dup B $R','gps') RETURNING id")
sql "ALTER TABLE app.devices ENABLE TRIGGER devices_gps_identity_unique" >/dev/null
[ -n "$DB" ] && ok "duplicato storico dell'IMEI nel tenant B predisposto" || no "setup duplicato" "-"

R1=$(B -X POST "$BASE/api/devices/$DB/command" -d '{"type":"posizione"}')
echo "$R1" | jv "d['ok']==False and 'non connesso' in d['result']" | grep -q True && ok "B comandi rapidi -> non raggiunge il socket di A" || no "B comandi rapidi" "$R1"
C=$(B -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DB/gprs-command" -d '{"command":"cpureset"}')
[ "$C" = "502" ] && ok "B comando GPRS grezzo -> non raggiunge il socket di A (502)" || no "B gprs" "$C"
R3=$(B -X POST "$BASE/api/iot/devices/$DB/command" -d '{"method":"setparam 2004:evil.example"}')
echo "$R3" | jv "d['status']=='error'" | grep -q True && ok "B RPC IoT -> non raggiunge il socket di A" || no "B rpc" "$R3"
C=$(O -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DB/command" -d '{"type":"personalizzato","data":"cpureset"}')
[ "$C" = "403" ] && ok "testo libero senza amministrazione dispositivi -> 403" || no "permesso testo libero" "$C"
C=$(O -o /dev/null -w '%{http_code}' -X POST "$BASE/api/iot/devices/$DB/command" -d '{"method":"cpureset"}')
[ "$C" = "403" ] && ok "RPC verso device GPS senza amministrazione dispositivi -> 403" || no "permesso rpc" "$C"
sleep 0.5
[ ! -s "$GOT" ] && ok "il badge di A non ha ricevuto alcun comando dal tenant B" || no "comandi trapelati" "$(cat "$GOT")"

R4=$(A -X POST "$BASE/api/devices/$DA/command" -d '{"type":"posizione"}')
echo "$R4" | jv "d['ok']==True and d['response']=='OK:getgps'" | grep -q True && ok "A comandi rapidi -> consegnato e risposto" || no "A comandi rapidi" "$R4"
R5=$(A -X POST "$BASE/api/devices/$DA/gprs-command" -d '{"command":"getver"}')
echo "$R5" | jv "d['response']=='OK:getver'" | grep -q True && ok "A comando GPRS grezzo -> consegnato" || no "A gprs" "$R5"
setm $TA iot attivo
R6=$(A -X POST "$BASE/api/iot/devices/$DA/command" -d '{"method":"getstatus"}')
echo "$R6" | jv "d['status']=='done' and d['result']=='OK:getstatus'" | grep -q True && ok "A RPC IoT -> consegnato sul proprio socket" || no "A rpc" "$R6"
setm $TA iot non_abilitato
[ "$(tr -d '\r' < "$GOT" | paste -sd, -)" = "getgps,getver,getstatus" ] && ok "il badge ha ricevuto esattamente i 3 comandi di A, in ordine" || no "comandi ricevuti" "$(cat "$GOT")"

AMB=$(TPORT=$TPORT IMEI=$IMEI python - <<'PY'
import socket,struct,os
imei=os.environ["IMEI"].encode()
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei); r=s.recv(1); print(r.hex() if r else "none"); s.close()
PY
)
[ "$AMB" = "00" ] && ok "nuova connessione con IMEI conteso tra tenant rifiutata (0x00)" || no "ambiguità" "$AMB"

DI=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TA','iot-$R','sensore','iot') RETURNING id")
C=$(A -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DI/command" -d '{"type":"posizione"}')
[ "$C" = "400" ] && ok "comandi GPRS rifiutati per device non GPS (400)" || no "source non gps" "$C"
PB_LW=$(prev $TB loneworker); setm $TB gps non_abilitato; setm $TB iot non_abilitato; setm $TB loneworker non_abilitato
C=$(B -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DB/command" -d '{"type":"posizione"}')
[ "$C" = "403" ] && ok "comandi bloccati senza moduli dispositivi attivi (403)" || no "gating moduli" "$C"
setm $TB loneworker "$PB_LW"; setm $TB gps attivo
kill $SIM 2>/dev/null; wait $SIM 2>/dev/null

# ============ 1b) titolarità CORRENTE dopo spostamento e riconnessione ==========================
SIMPY="$(dirname "$0")/lib/telt_sim.py"
IM2=$(python -c "import random;print('358'+str(random.randint(10**11,10**12-1)))")
T=$(mktemp -d)
sim(){ python "$SIMPY" --imei "$IM2" --port "$TPORT" --ready "$T/ready$1" --got "$T/got$1" --ctl "$T/ctl$1" --res "$T/res$1" --secs 40 & }
waitf(){ for i in $(seq 1 75); do [ -s "$1" ] && break; sleep 0.2; done; cat "$1" 2>/dev/null; }
DX=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TA','$IM2','sposta $R','gps') RETURNING id")
sim 1; S1=$!
[ "$(waitf "$T/ready1")" = "accepted" ] && ok "badge collegato per il tenant A" || no "sim1" "$(cat "$T/ready1" 2>/dev/null)"
A -X POST "$BASE/api/devices/$DX/command" -d '{"type":"stato"}' | jv "d['ok']" | grep -q True && ok "A comanda il proprio badge" || no "A cmd"
# spostamento: il device passa al tenant B (cancellato in A, censito in B: la titolarità si libera)
sql "DELETE FROM app.devices WHERE id='$DX'" >/dev/null
DY=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TB','$IM2','arrivato $R','gps') RETURNING id")
[ -n "$DY" ] && ok "IMEI liberato da A e registrabile da B (titolarità aggiornata)" || no "rilascio titolarità"
C=$(A -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DX/command" -d '{"type":"stato"}')
[ "$C" = "404" ] && ok "A non comanda più il badge spostato (404)" || no "A dopo spostamento" "$C"
B -X POST "$BASE/api/devices/$DY/command" -d '{"type":"stato"}' | jv "d['ok']==False" | grep -q True   && ok "B non usa la connessione aperta per A (non connesso finché non riconnette)" || no "B su connessione vecchia"
touch "$T/ctl1"; [ "$(waitf "$T/res1")" = "closed" ] && ok "al pacchetto successivo il bridge chiude la connessione del titolare precedente" || no "chiusura" "$(cat "$T/res1" 2>/dev/null)"
wait $S1 2>/dev/null
sim 2; S2=$!
[ "$(waitf "$T/ready2")" = "accepted" ] && ok "riconnessione: badge associato al nuovo titolare B" || no "sim2" "$(cat "$T/ready2" 2>/dev/null)"
B -X POST "$BASE/api/devices/$DY/command" -d '{"type":"versione"}' | jv "d['response']=='OK:getver'" | grep -q True && ok "B ora comanda il badge" || no "B dopo riconnessione"
# riassegnazione a un altro lavoratore nello stesso tenant: la titolarità non cambia
UB=$(one "SELECT id FROM app.users WHERE tenant_id='$TB' AND role='lavoratore' LIMIT 1")
B -o /dev/null -X POST "$BASE/api/devices/$DY/assign" -d "{\"user_id\":\"$UB\"}"
B -X POST "$BASE/api/devices/$DY/command" -d '{"type":"info"}' | jv "d['response']=='OK:getinfo'" | grep -q True && ok "dopo la riassegnazione a un lavoratore i comandi continuano" || no "dopo riassegnazione"
# ricreato nello stesso tenant (nuovo device id): la connessione vecchia non serve il device nuovo
sql "DELETE FROM app.devices WHERE id='$DY'" >/dev/null
DZ=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TB','$IM2','ricreato $R','gps') RETURNING id")
B -X POST "$BASE/api/devices/$DZ/command" -d '{"type":"stato"}' | jv "d['ok']==False" | grep -q True && ok "device ricreato: la connessione del device precedente non viene usata" || no "device ricreato"
touch "$T/ctl2"; [ "$(waitf "$T/res2")" = "closed" ] && ok "connessione del device precedente chiusa al pacchetto successivo" || no "chiusura 2"
wait $S2 2>/dev/null
sim 3; S3=$!; waitf "$T/ready3" >/dev/null
B -X POST "$BASE/api/devices/$DZ/command" -d '{"type":"stato"}' | jv "d['response']=='OK:getstatus'" | grep -q True && ok "dopo la riconnessione il device nuovo è comandabile" || no "device nuovo"
touch "$T/ctl3"; wait $S3 2>/dev/null
[ "$(tr -d '\r' < "$T/got1" | paste -sd, -)" = "getstatus" ] && [ "$(tr -d '\r' < "$T/got2" | paste -sd, -)" = "getver,getinfo" ] && ok "ogni connessione ha ricevuto solo i comandi del titolare corrente" || no "comandi per connessione" "$(cat "$T"/got* 2>/dev/null | tr '\n' ' ')"

# ============ 2) ingest JSON con loneworker+gps, iot spento ======================================
DJ=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user) VALUES('$TA','json-$R','badge json','gps','$UA') RETURNING id")
TOK="e2e-iso-$R"; sql "INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id) VALUES('$TOK',NULL,'$TA','gps','$DJ')" >/dev/null
[ "$(prev $TA iot)" = "non_abilitato" ] && ok "tenant A: loneworker+gps attivi, iot spento" || no "moduli" "-"
C=$(code -X POST "$BASE/api/ingress/gps/position?token=$TOK" -H 'content-type: application/json' -d "{\"position\":{\"id\":\"p-$R\",\"latitude\":45.2,\"longitude\":9.2}}")
[ "$C" = "200" ] && [ -n "$(one "SELECT last_pos_at FROM app.devices WHERE id='$DJ'")" ] && ok "posizione JSON accettata senza modulo IoT" || no "posizione json" "$C"
SOS=$(curl -s -X POST "$BASE/api/ingress/gps?token=$TOK" -H 'content-type: application/json' -d "{\"event\":{\"id\":\"sos-$R\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"position\":{\"latitude\":45.2,\"longitude\":9.2}}")
N=$(one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DJ' AND i.etype='sos'")
[ "$N" = "1" ] && ok "SOS JSON -> incidente sos senza modulo IoT" || no "sos json" "$SOS"

# ============ 3) presenza OsmAnd e watchdog (contatto ≠ posizione) ===============================
ADV(){ code -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}" >/dev/null; }
WD(){ code -X POST "$BASE/api/_test/gps-silence-watchdog" >/dev/null; }
NPC(){ one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DJ' AND i.etype='perdita_contatto'"; }
# il watchdog vale solo per i badge IN SERVIZIO (mig 0128): messa in servizio esplicita
# Q4 (25/11/2026): e solo durante il turno -> turno aperto per il lavoratore (chiuso a fine test)
SHIFT_J=$(one "INSERT INTO app.shifts(tenant_id,user_id,started_at) SELECT '$TA','$UA','2000-01-01T00:00:00Z'
               WHERE NOT EXISTS (SELECT 1 FROM app.shifts WHERE tenant_id='$TA' AND user_id='$UA' AND state='in_corso') RETURNING id")
C=$(A -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DJ/service" -d '{"state":"in_servizio"}')
[ "$C" = "200" ] && ok "badge JSON messo in servizio" || no "messa in servizio" "$C"
POS0=$(one "SELECT last_pos_at FROM app.devices WHERE id='$DJ'")
ADV 500
C=$(code "$BASE/api/ingress/osmand?token=$TOK&batt=77")
SEEN=$(one "SELECT last_seen_at > last_pos_at FROM app.devices WHERE id='$DJ'")
[ "$C" = "200" ] && [ "$SEEN" = "t" ] && [ "$(one "SELECT last_pos_at FROM app.devices WHERE id='$DJ'")" = "$POS0" ] \
  && ok "OsmAnd senza coordinate: presenza aggiornata, posizione invariata" || no "heartbeat osmand" "$C seen>pos=$SEEN"
ADV 500; WD
[ "$(NPC)" = "0" ] && ok "posizione vecchia 1000s ma heartbeat 500s fa -> nessun allarme" || no "falso allarme" "$(NPC)"
ADV 200; WD
[ "$(NPC)" = "1" ] && ok "heartbeat oltre 600s -> UN perdita_contatto" || no "watchdog contatto" "$(NPC)"
PL=$(one "SELECT (e.payload_ref->>'last_pos_at')::timestamptz < (e.payload_ref->>'last_seen_at')::timestamptz FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DJ' AND i.etype='perdita_contatto'")
[ "$PL" = "t" ] && ok "l'allarme riporta età posizione e età ultimo contatto distinte" || no "payload" "$PL"

[ -n "${SHIFT_J:-}" ] && sql "DELETE FROM app.shifts WHERE id='$SHIFT_J'" >/dev/null

# ============ 4) configurazione strumento + scheda dispositivo ===================================
sql "INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config) VALUES('$TA','gps','alarm_map','{\"events\":{\"deviceOverspeed\":\"critico\"}}')
     ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config = app.tenant_module_config.config || EXCLUDED.config" >/dev/null
S1=$(A -X POST "$BASE/api/modules/gps/tools/alarm_map" -d '{"config":{"silence_after_s":120}}')
DBC=$(sql "SELECT config::text FROM app.tenant_module_config WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'")
python -c "import json,sys;a=json.loads(sys.argv[1])['config'];b=json.loads(sys.argv[2]);sys.exit(0 if a==b else 1)" "$S1" "$DBC" \
  && ok "la risposta riporta la configurazione realmente salvata" || no "eco config" "$S1 vs $DBC"
[ "$(one "SELECT config->'events'->>'deviceOverspeed' FROM app.tenant_module_config WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'")" = "critico" ] \
  && ok "policy eventi (gestita altrove) preservata al salvataggio" || no "events persi"
LS=$(A "$BASE/api/devices" | jv "[x['silence_after_s'] for x in d if x['id']=='$DJ'][0]")
[ "$LS" = "120" ] && ok "scheda dispositivo riceve la soglia reale del tenant (120s)" || no "soglia in scheda" "$LS"
A -X POST "$BASE/api/modules/gps/tools/alarm_map" -d '{"config":{}}' >/dev/null
[ "$(one "SELECT config ? 'silence_after_s' FROM app.tenant_module_config WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'")" = "f" ] \
  && [ "$(A "$BASE/api/devices" | jv "[x['silence_after_s'] for x in d if x['id']=='$DJ'][0]")" = "600" ] \
  && ok "campo omesso -> torna al default (600s)" || no "reset al default" "-"

# ============ 5) report parziale ================================================================
DR=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TA','rep-$R','report','gps') RETURNING id")
sql "INSERT INTO app.device_positions(tenant_id,device_id,lat,lon,fixed_at,source_id)
     SELECT '$TA','$DR',45.0+(g%200)*0.0001,9.0,now()-interval '2 days'+g*interval '2 seconds','rep-$R-'||g FROM generate_series(1,50001) g" >/dev/null
FROM=$(python -c "import datetime;print((datetime.datetime.now(datetime.UTC).replace(tzinfo=None)-datetime.timedelta(days=3)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TO=$(python -c "import datetime;print(datetime.datetime.now(datetime.UTC).replace(tzinfo=None).strftime('%Y-%m-%dT%H:%M:%SZ'))")
E=$(A "$BASE/api/devices/$DR/report/summary?from=$FROM&to=$TO&envelope=1")
echo "$E" | jv "d['partial']==True and d['summary']['points']==50000 and d['covered_until'] is not None" | grep -q True \
  && ok "50.001 posizioni: riepilogo dichiarato parziale (50.000 punti, coperto fino a…)" || no "parziale json" "$(echo "$E" | cut -c1-200)"
HD=$(curl -s -D - -o /dev/null -b "$AJ" -H "x-tenant-slug: globex" "$BASE/api/devices/$DR/report/trips?from=$FROM&to=$TO" | tr -d '\r' | grep -i '^x-yw-report-partial:' | awk '{print $2}')
[ "$HD" = "true" ] && ok "header x-yw-report-partial: true (forma storica invariata)" || no "header parziale" "$HD"
A "$BASE/api/devices/$DR/report/trips?from=$FROM&to=$TO" | jv "isinstance(d,list)" | grep -q True && ok "senza envelope la risposta resta un array" || no "compatibilità"
curl -s -b "$AJ" -H "x-tenant-slug: globex" "$BASE/api/devices/$DR/report/trips?from=$FROM&to=$TO&csv=true" | tail -1 | grep -q "ATTENZIONE: risultato PARZIALE" \
  && ok "CSV parziale contiene l'avviso" || no "avviso csv"
E2=$(A "$BASE/api/devices/$DJ/report/summary?from=$FROM&to=$TO&envelope=1")
echo "$E2" | jv "d['partial']==False" | grep -q True && ok "sotto il limite il report è completo (partial=false)" || no "completo" "$E2"

# ============ teardown ==========================================================================
for D in "$DA" "$DB" "$DJ" "$DI" "$DR" "$DZ"; do
  sql "DELETE FROM app.incidents i USING app.events e WHERE i.event_id=e.id AND e.device_id='$D'" >/dev/null 2>&1
  sql "DELETE FROM app.devices WHERE id='$D'" >/dev/null 2>&1
done
sql "DELETE FROM app.incidents WHERE correlation LIKE 'telt-$IMEI-%'" >/dev/null 2>&1
sql "DELETE FROM app.ingest_tokens WHERE token='$TOK'" >/dev/null
if [ -n "$PA_CFG" ]; then sql "UPDATE app.tenant_module_config SET config='$PA_CFG'::jsonb WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'" >/dev/null
else sql "DELETE FROM app.tenant_module_config WHERE tenant_id='$TA' AND module_code='gps' AND tool_code='alarm_map'" >/dev/null; fi
setm $TA gps "$PA_GPS"; setm $TA loneworker "$PA_LW"; setm $TA iot "$PA_IOT"; setm $TB gps "$PB_GPS"; setm $TB iot "$PB_IOT"
[ "$(one "SELECT count(*) FROM app.devices WHERE external_id IN ('$IMEI','$IM2','json-$R','iot-$R','rep-$R')")" = "0" ] && ok "teardown completo" || no "teardown"
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
