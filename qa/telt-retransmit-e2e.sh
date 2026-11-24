#!/usr/bin/env bash
# SL-4: ritrasmissioni del badge Teltonika e SOS ripetuti.
#  - lo STESSO record AVL ritrasmesso (stesso IMEI, orario e allarme: il badge non ha visto l'ACK)
#    non apre un secondo incidente
#  - un nuovo SOS mentre il primo è ancora aperto si annota sullo stesso incidente
#  - dopo la chiusura, un nuovo SOS apre un nuovo incidente
# Tenant globex (moduli ripristinati), device e incidenti di prova rimossi a fine suite.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; SLUG="globex"
TELT_HOST="${TELT_HOST:-127.0.0.1}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
GPS_PREV=$(one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='gps'),'non_abilitato')")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','gps','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
IMEI=$(python -c "import random;print('357'+str(random.randint(10**11,10**12-1)))")
DID=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source,model_code) VALUES('$TID','$IMEI','Ritrasmissione test','gps','teltonika-gh-5200') RETURNING id")
cleanup(){
  sql "DELETE FROM app.incidents i USING app.events e WHERE i.event_id=e.id AND e.device_id='$DID'" >/dev/null 2>&1
  sql "DELETE FROM app.devices WHERE id='$DID'" >/dev/null 2>&1
  sql "UPDATE app.tenant_modules SET state='$GPS_PREV' WHERE tenant_id='$TID' AND module_code='gps'" >/dev/null 2>&1; }
trap cleanup EXIT
export TPORT IMEI TELT_HOST

# invia N volte lo STESSO pacchetto (ts fisso TS_MS) con evento IO 236 (SOS sul GH-5200)
send(){ TS_MS="$1" TIMES="$2" python - <<'PY'
import socket,struct,os
imei=os.environ["IMEI"].encode(); ts=int(os.environ["TS_MS"]); times=int(os.environ["TIMES"])
s=socket.create_connection((os.environ["TELT_HOST"],int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei); assert s.recv(1)==b"\x01","not accepted"
rec=struct.pack(">Q",ts)+b"\x01"+struct.pack(">ii",int(9.19e7),int(45.47e7))+struct.pack(">HHBH",0,0,8,0)
rec+=bytes([236])+b"\x01"+b"\x01"+bytes([236,1])+b"\x00\x00\x00"
data=b"\x08"+b"\x01"+rec+b"\x01"
pkt=b"\x00\x00\x00\x00"+struct.pack(">I",len(data))+data+b"\x00\x00\x00\x00"
acks=[]
for _ in range(times):
    s.sendall(pkt); a=s.recv(4); acks.append(struct.unpack(">I",a)[0] if len(a)==4 else -1)
print(",".join(map(str,acks))); s.close()
PY
}
nsos(){ one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.tenant_id='$TID' AND e.device_id='$DID' AND i.etype='sos'"; }
T0=$(python -c "import time;print(int(time.time()*1000))")

A=$(send $T0 3); sleep 1
[ "$A" = "1,1,1" ] && ok "badge: tre invii dello stesso record, tutti confermati" || no "ack" "$A"
[ "$(nsos)" = "1" ] && ok "record ritrasmesso: un solo incidente SOS" || no "ritrasmissione" "$(nsos)"
send $((T0+60000)) 1 >/dev/null; sleep 1
[ "$(nsos)" = "1" ] && ok "nuovo SOS con incidente aperto: nessun secondo incidente" || no "ripetuto aperto" "$(nsos)"
IID=$(one "SELECT i.id FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID' AND i.etype='sos' LIMIT 1")
[ "$(one "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$IID' AND kind='dedup_suppressed'")" -ge 1 ] \
  && ok "il SOS ripetuto è annotato sull'incidente aperto" || no "annotazione" "-"
sql "UPDATE app.incidents SET state='chiuso', closed_at=now() WHERE id='$IID'" >/dev/null
send $((T0+120000)) 1 >/dev/null; sleep 1
N=$(one "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND correlation='telt-$IMEI-sos'")
[ "$N" = "2" ] && ok "dopo la chiusura un nuovo SOS apre un nuovo incidente" || no "dopo chiusura" "$N"

echo "telt-retransmit: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
