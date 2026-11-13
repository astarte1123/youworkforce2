#!/usr/bin/env bash
# Edge nativo Teltonika — server TCP Codec8/8E. Prova: IMEI sconosciuto rifiutato; IMEI censito
# accettato; pacchetto Codec8 con posizione -> ultima posizione aggiornata; Codec8 Extended ->
# idem; event IO mappato -> allarme/incidente. Decode-edge senza Traccar.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
# stato moduli PRIMA (per ripristino: evita di inquinare altre suite, es. modularity assume traccar OFF)
TR_PREV=$(one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='traccar'),'non_abilitato')")
LW_PREV=$(one "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$TID' AND module_code='loneworker'),'non_abilitato')")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','traccar','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','loneworker','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
R=$RANDOM; IMEI="35612300000$R"; IMEI=${IMEI:0:15}
# device con source=traccar (decode-edge) e external_id=IMEI
DID=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TID','$IMEI','GH5200 $R','traccar') RETURNING id")
# event IO 1 => sos via mappa di DEFAULT built-in (nessuna config necessaria)
export TPORT IMEI

# 1) IMEI sconosciuto -> reject (0x00)
UNK=$(python - <<'PY'
import socket,struct,os
imei=b"999999999999999"
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei)
r=s.recv(1); print(r.hex() if r else "none"); s.close()
PY
)
[ "$UNK" = "00" ] && ok "IMEI sconosciuto rifiutato (0x00)" || no "reject ko" "$UNK"

# 2) IMEI censito -> accept (0x01) + pacchetto Codec8 posizione -> ack 1
ACKPOS=$(python - <<'PY'
import socket,struct,os,time
imei=os.environ["IMEI"].encode()
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei)
a=s.recv(1)
assert a==b"\x01", "not accepted: %r"%a
# un record Codec8: lat 45.4642 lon 9.1900, event IO 0 (no alarm)
ts=int(time.time()*1000)
lon=int(9.1900*1e7); lat=int(45.4642*1e7)
rec=struct.pack(">Q",ts)+b"\x00"            # timestamp + priority
rec+=struct.pack(">ii",lon,lat)             # lon,lat (signed)
rec+=struct.pack(">HHBH",100,90,8,0)        # altitude,angle,sat,speed
rec+=b"\x00"                                 # event io id = 0
rec+=b"\x00"                                 # total io
rec+=b"\x00\x00\x00\x00"                     # N1,N2,N4,N8 = 0
data=b"\x08"+b"\x01"+rec+b"\x01"            # codec8, count1, rec, count1
pkt=b"\x00\x00\x00\x00"+struct.pack(">I",len(data))+data+b"\x00\x00\x00\x00"
s.sendall(pkt)
ack=s.recv(4); print(struct.unpack(">I",ack)[0] if len(ack)==4 else -1); s.close()
PY
)
[ "$ACKPOS" = "1" ] && ok "IMEI censito accettato + ack 1 record (Codec8)" || no "codec8 ack ko" "$ACKPOS"
sleep 1
# ultima posizione aggiornata sul device
LP=$(one "SELECT round(lat::numeric,3)||','||round(lon::numeric,3) FROM app.device_positions WHERE device_id='$DID' ORDER BY received_at DESC LIMIT 1" 2>/dev/null)
[ -z "$LP" ] && LP=$(one "SELECT round(last_lat::numeric,3)||','||round(last_lon::numeric,3) FROM app.devices WHERE id='$DID'" 2>/dev/null)
echo "$LP" | grep -q "45.464" && ok "posizione ingerita (last pos ~45.464,9.190)" || no "pos ko" "$LP"

# 3) Codec8 Extended con event IO 1 -> allarme -> incidente
ACKE=$(python - <<'PY'
import socket,struct,os,time
imei=os.environ["IMEI"].encode()
s=socket.create_connection(("127.0.0.1",int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei); assert s.recv(1)==b"\x01"
ts=int(time.time()*1000); lon=int(9.19*1e7); lat=int(45.47*1e7)
rec=struct.pack(">Q",ts)+b"\x01"
rec+=struct.pack(">ii",lon,lat)+struct.pack(">HHBH",0,0,7,0)
rec+=struct.pack(">H",1)      # event io id = 1 (ext = 2 byte)
rec+=struct.pack(">H",0)      # total io = 0
rec+=struct.pack(">H",0)*4    # N1..N8 = 0 (2-byte each in ext)
rec+=struct.pack(">H",0)      # NX = 0
data=b"\x8e"+b"\x01"+rec+b"\x01"
pkt=b"\x00\x00\x00\x00"+struct.pack(">I",len(data))+data+b"\x00\x00\x00\x00"
s.sendall(pkt); ack=s.recv(4); print(struct.unpack(">I",ack)[0] if len(ack)==4 else -1); s.close()
PY
)
[ "$ACKE" = "1" ] && ok "Codec8 Extended ack 1 record" || no "codec8e ack ko" "$ACKE"
sleep 1
INC=$(one "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND correlation LIKE 'telt-%'")
[ "${INC:-0}" -ge 1 ] && ok "event IO mappato -> incidente (sos)" || no "alarm ko" "inc=$INC"

# teardown
sql "DELETE FROM app.incidents WHERE tenant_id='$TID' AND correlation LIKE 'telt-%'" >/dev/null 2>&1 || true
sql "DELETE FROM app.device_positions WHERE device_id='$DID'" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices WHERE id='$DID'" >/dev/null
# ripristino stato moduli
sql "UPDATE app.tenant_modules SET state='$TR_PREV' WHERE tenant_id='$TID' AND module_code='traccar'" >/dev/null
sql "UPDATE app.tenant_modules SET state='$LW_PREV' WHERE tenant_id='$TID' AND module_code='loneworker'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
