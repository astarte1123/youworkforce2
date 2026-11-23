#!/usr/bin/env bash
# GH-5200 (Teltonika) percorso SALVA-VITA end-to-end sul perimetro nativo.
# Prova che i segnali reali del tracker personale diventano incidenti CRITICI YW:
#   - IO 242 (ManDown/FallDown) -> incidente 'man_down' (uomo a terra / brusca caduta)
#   - IO 236 (pulsante SOS)     -> incidente 'sos'   (via io_alarm_map del MODELLO, non 'alarm')
#   - posizione GPS ingerita
# La mappa IO->allarme viene dal catalogo (device_models.default_config.io_alarm_map), quindi
# il device deve avere model_code='teltonika-gh-5200'. Tenant globex (isolato).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; SLUG="globex"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
TR_PREV=$(one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='gps'),'non_abilitato')")
LW_PREV=$(one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='loneworker'),'non_abilitato')")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','gps','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','loneworker','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null

# device GH-5200 con model_code (così si applica io_alarm_map del modello: 236->sos, 242->man_down)
IMEI=$(python -c "import random;print('356'+str(random.randint(10**11,10**12-1)))")
DID=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source,model_code) VALUES('$TID','$IMEI','GH5200 test','gps','teltonika-gh-5200') RETURNING id")
[ -n "$DID" ] && ok "GH-5200 onboarded (model_code=teltonika-gh-5200)" || { no "device non creato"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }
export TPORT IMEI

# helper python: invia UN record Codec8 con event_io dato (+posizione). Stampa l'ack numerico.
send_io(){ EVIO="$1" LATF="$2" python - <<'PY'
import socket,struct,os,time
imei=os.environ["IMEI"].encode(); evio=int(os.environ["EVIO"]); latf=float(os.environ["LATF"])
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei)
assert s.recv(1)==b"\x01","not accepted"
ts=int(time.time()*1000); lon=int(9.1900*1e7); lat=int(latf*1e7)
rec=struct.pack(">Q",ts)+b"\x01"+struct.pack(">ii",lon,lat)+struct.pack(">HHBH",0,0,8,0)
rec+=bytes([evio])            # event io id (1 byte, Codec8)
rec+=b"\x01"                  # total io = 1
rec+=b"\x01"+bytes([evio,1])  # N1=1: id=evio val=1 (evento attivo)
rec+=b"\x00\x00\x00"          # N2,N4,N8 = 0
data=b"\x08"+b"\x01"+rec+b"\x01"
pkt=b"\x00\x00\x00\x00"+struct.pack(">I",len(data))+data+b"\x00\x00\x00\x00"
s.sendall(pkt); ack=s.recv(4); print(struct.unpack(">I",ack)[0] if len(ack)==4 else -1); s.close()
PY
}

inc_count(){ one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.tenant_id='$TID' AND e.device_id='$DID' AND i.etype='$1'"; }

# 1) ManDown/FallDown (IO 242) -> incidente man_down
A1=$(send_io 242 45.4642)
[ "$A1" = "1" ] && ok "GH-5200 invia ManDown (IO 242), ack ok" || no "ack 242 ko" "$A1"
sleep 1
MD=$(inc_count man_down)
[ "${MD:-0}" -ge 1 ] && ok "IO 242 -> incidente CRITICO 'man_down' (uomo a terra/caduta)" || no "man_down non creato" "count=$MD"

# 2) SOS button (IO 236) -> incidente sos (via mappa MODELLO, non 'alarm')
A2=$(send_io 236 45.4650)
[ "$A2" = "1" ] && ok "GH-5200 invia SOS (IO 236), ack ok" || no "ack 236 ko" "$A2"
sleep 1
SOS=$(inc_count sos)
[ "${SOS:-0}" -ge 1 ] && ok "IO 236 -> incidente CRITICO 'sos' (mappa modello)" || no "sos non creato" "count=$SOS"
# e NON deve essere stato classificato come 'alarm'/'device_alarm' generico
GEN=$(inc_count device_alarm)
[ "${GEN:-0}" = "0" ] && ok "nessun fallback 'device_alarm' (semantica salva-vita corretta)" || no "fallback generico presente" "count=$GEN"

# 3) posizione GPS ingerita
sleep 1
LP=$(one "SELECT round(last_lat::numeric,3)||','||round(last_lon::numeric,3) FROM app.devices WHERE id='$DID'")
echo "$LP" | grep -q "45.46" && ok "posizione GPS ingerita ($LP)" || no "pos ko" "$LP"

# teardown completo (device eliminabile anche con eventi/incidenti collegati)
sql "DELETE FROM app.incidents i USING app.events e WHERE i.event_id=e.id AND e.device_id='$DID'" >/dev/null 2>&1 || true
sql "DELETE FROM app.incidents WHERE tenant_id='$TID' AND correlation LIKE 'telt-$IMEI-%'" >/dev/null 2>&1 || true
DELRC=$(sql "DELETE FROM app.devices WHERE id='$DID'; SELECT count(*) FROM app.devices WHERE id='$DID'")
[ "$(echo "$DELRC" | tr -d '\r' | tail -1)" = "0" ] && ok "device eliminabile del tutto (no fantasma)" || no "device non eliminato" "$DELRC"
sql "UPDATE app.tenant_modules SET state='$TR_PREV' WHERE tenant_id='$TID' AND module_code='gps'" >/dev/null
sql "UPDATE app.tenant_modules SET state='$LW_PREV' WHERE tenant_id='$TID' AND module_code='loneworker'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
