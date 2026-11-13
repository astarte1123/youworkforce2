#!/usr/bin/env bash
# Teltonika comando GPRS (codec12): il server invia un comando al device CONNESSO sul socket TCP
# aperto e attende la risposta. Prova: device connesso riceve il comando e risponde; device non
# connesso -> 502.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','traccar','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
R=$RANDOM; IMEI="35612399000$R"; IMEI=${IMEI:0:15}
DID=$(one "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TID','$IMEI','FMB $R','traccar') RETURNING id")

AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$ADMIN\",\"password\":\"demo\"}"
ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }

# device NON connesso -> 502
C=$(adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/gprs-command" -d '{"command":"getver"}')
[ "$C" = "502" ] && ok "device non connesso -> 502" || no "offline ko" "$C"

# device simulato connesso: resta in ascolto, risponde al comando
READY=$(mktemp); GOT=$(mktemp)
TPORT=$TPORT IMEI=$IMEI READY=$READY GOT=$GOT python - <<'PY' &
import socket,struct,os
imei=os.environ["IMEI"].encode(); P=int(os.environ["TPORT"])
s=socket.create_connection(("127.0.0.1",P),timeout=15)
s.sendall(struct.pack(">H",len(imei))+imei); assert s.recv(1)==b"\x01"
open(os.environ["READY"],"w").write("1")
# leggi un pacchetto codec12 comando: preamble4+len4+inner+crc4
h=s.recv(8); dl=struct.unpack(">I",h[4:8])[0]; body=b""
while len(body)<dl+4: body+=s.recv(dl+4-len(body))
data=body[:dl]
# inner: [0x0C][qty1][0x05][size4][cmd][qty2]
assert data[0]==0x0C and data[2]==0x05
size=struct.unpack(">I",data[3:7])[0]; cmd=data[7:7+size].decode()
open(os.environ["GOT"],"w").write(cmd)
# rispondi codec12 type 0x06
resp=("OK:"+cmd).encode()
inner=bytes([0x0C,0x01,0x06])+struct.pack(">I",len(resp))+resp+bytes([0x01])
# crc16 IBM
def crc16(d):
  c=0
  for b in d:
    c^=b
    for _ in range(8): c=(c>>1)^0xA001 if c&1 else c>>1
  return c
pkt=b"\x00\x00\x00\x00"+struct.pack(">I",len(inner))+inner+struct.pack(">I",crc16(inner))
s.sendall(pkt)
import time; time.sleep(2); s.close()
PY
DEVPID=$!
# attendi ready
for i in $(seq 1 30); do [ -s "$READY" ] && break; sleep 0.2; done
[ -s "$READY" ] && ok "device simulato connesso" || no "connect ko" "-"

# invia comando GPRS via API -> attende risposta dal device
RESP=$(adm -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/gprs-command" -d '{"command":"getinfo"}')
echo "$RESP" | grep -q '"response":"OK:getinfo"' && ok "comando inviato e risposta ricevuta (OK:getinfo)" || no "cmd ko" "$RESP"
sleep 1
[ "$(cat "$GOT" 2>/dev/null)" = "getinfo" ] && ok "device ha ricevuto il comando esatto" || no "got ko" "$(cat "$GOT" 2>/dev/null)"

wait $DEVPID 2>/dev/null || true
# teardown
sql "DELETE FROM app.device_positions WHERE device_id='$DID'" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices WHERE id='$DID'" >/dev/null
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='traccar'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
