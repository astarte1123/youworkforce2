#!/usr/bin/env bash
# Badge rifiutati + collegamento live (mig 0136).
#  - IMEI non censito: rifiutato e REGISTRATO (motivo, contatore), visibile solo al superadmin
#  - IMEI censito poi accettato: la riga sparisce; la scheda dispositivo dice «collegato adesso»
#    finché la connessione TCP è aperta, poi non più
#  - IMEI censito in due aziende: rifiutato come «conteso», l'admin del tenant lo vede sul SUO badge
#  - azienda senza modulo dispositivi: rifiutato come «modulo spento»
# Tenant usa-e-getta. Richiede il bridge Teltonika in ascolto (YW_TELTONIKA_PORT, default 5027).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TPORT="${TPORT:-5027}"; THOST="${THOST:-127.0.0.1}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))')
mk(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$1\",\"name\":\"Da $1\",\"admin_email\":\"a@$1.test\",\"admin_password\":\"DirectPass9\",\"modules\":[$2]}" | py "d.get('id','')"; }
SA_="da$R"; SB_="db$R"; SC_="dc$R"
TA=$(mk "$SA_" '"gps"'); TB=$(mk "$SB_" '"gps"'); TC=$(mk "$SC_" '"interventi"')
del(){ curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$1" -d "{\"slug\":\"$2\"}"; }
IMEI1=$(python -c "import random;print('357'+str(random.randint(10**11,10**12-1)))")
IMEI2=$(python -c "import random;print('358'+str(random.randint(10**11,10**12-1)))")
IMEI3=$(python -c "import random;print('359'+str(random.randint(10**11,10**12-1)))")
trap 'del "$TA" "$SA_"; del "$TB" "$SB_"; del "$TC" "$SC_"; docker exec -i ywdb psql -U postgres -d youworkforce -qc "DELETE FROM app.device_connect_attempts WHERE identity IN ('"'"'$IMEI1'"'"','"'"'$IMEI2'"'"','"'"'$IMEI3'"'"')" >/dev/null; rm -rf "$J"' EXIT
[ -n "$TA" ] && [ -n "$TB" ] && [ -n "$TC" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $1" -d "{\"email\":\"a@$1.test\",\"password\":\"DirectPass9\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $1" -H "x-csrf-token: $c" -H 'content-type: application/json' "$BASE$2"; }
login "$SA_"
export TPORT THOST
# handshake: stampa l'esito (01/00); con HOLD=s tiene aperta la connessione s secondi
hs(){ IMEI="$1" HOLD="${2:-0}" python - <<'PY'
import socket,struct,os,time
imei=os.environ["IMEI"].encode()
s=socket.create_connection((os.environ["THOST"],int(os.environ["TPORT"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei)
r=s.recv(1); print(r.hex() if r else "none", flush=True)
time.sleep(float(os.environ["HOLD"])); s.close()
PY
}
row(){ q "SELECT reason||':'||attempts FROM app.device_connect_attempts WHERE protocol='teltonika' AND identity='$1'"; }

echo "== IMEI non censito =="
[ "$(hs "$IMEI1")" = "00" ] && ok "IMEI non censito rifiutato" || no "reject" "-"
[ "$(row "$IMEI1")" = "sconosciuto:1" ] && ok "tentativo registrato (non censito, 1)" || no "registro" "$(row "$IMEI1")"
hs "$IMEI1" >/dev/null
[ "$(row "$IMEI1")" = "sconosciuto:2" ] && ok "secondo tentativo: contatore 2, una riga sola" || no "contatore" "$(row "$IMEI1")"
L=$(curl -s -b "$J/sa" "$BASE/api/superadmin/device-attempts")
echo "$L" | py "[r for r in d if r['identity']=='$IMEI1'][0]['reason_label']" | grep -q "non censito" \
  && ok "superadmin vede il tentativo con motivo leggibile" || no "superadmin lista" "$(echo "$L" | head -c 200)"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/$SA_" -H "x-tenant-slug: $SA_" "$BASE/api/superadmin/device-attempts")
[ "$C" = "401" ] || [ "$C" = "403" ] && ok "admin del tenant non vede il registro di sistema ($C)" || no "acl" "$C"

echo "== censito e collegato =="
DID=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TA','$IMEI1','Badge $R','gps') RETURNING id" | head -1 | tr -d '[:space:]')
hs "$IMEI1" 6 > "$J/hold.out" &
HP=$!
sleep 2
[ "$(head -1 "$J/hold.out")" = "01" ] && ok "IMEI censito accettato" || no "accept" "$(cat "$J/hold.out")"
[ -z "$(row "$IMEI1")" ] && ok "accettato: il tentativo rifiutato sparisce" || no "pulizia" "$(row "$IMEI1")"
call "$SA_" /api/devices | py "[x for x in d if x['id']=='$DID'][0]['connected_since'] is not None" | grep -q True \
  && ok "scheda: collegato adesso (connessione aperta)" || no "live" "-"
wait $HP; sleep 1
call "$SA_" /api/devices | py "[x for x in d if x['id']=='$DID'][0]['connected_since'] is None" | grep -q True \
  && ok "connessione chiusa: non più «collegato adesso»" || no "live off" "-"

echo "== IMEI conteso (dato storico: dal mig 0128 il DB impedisce i duplicati) =="
docker exec -i ywdb psql -U postgres -d youworkforce -q >/dev/null <<SQL
SET session_replication_role = replica;
INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TA','$IMEI2','Doppio A','gps');
INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TB','$IMEI2','Doppio B','gps');
SQL
[ "$(hs "$IMEI2")" = "00" ] && ok "IMEI in due aziende rifiutato" || no "conteso" "-"
[ "$(row "$IMEI2")" = "conteso:1" ] && ok "registrato come conteso" || no "conteso reg" "$(row "$IMEI2")"
call "$SA_" /api/devices | py "[x for x in d if x['external_id']=='$IMEI2'][0]['rejected']['reason']" | grep -q "^conteso$" \
  && ok "l'admin vede sul proprio badge il rifiuto e il motivo" || no "rejected scheda" "-"
curl -s -b "$J/sa" "$BASE/api/superadmin/device-attempts" | py "[r for r in d if r['identity']=='$IMEI2'][0]['tenants']" | grep -q "Da $SA_" \
  && ok "superadmin vede in quali aziende è censito" || no "tenants" "-"

echo "== azienda senza modulo dispositivi =="
q "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TC','$IMEI3','Senza modulo','gps')" >/dev/null
[ "$(hs "$IMEI3")" = "00" ] && ok "badge di azienda senza modulo dispositivi rifiutato" || no "modulo" "-"
[ "$(row "$IMEI3")" = "modulo_spento:1" ] && ok "registrato come modulo spento" || no "modulo reg" "$(row "$IMEI3")"

echo "device-attempts: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
