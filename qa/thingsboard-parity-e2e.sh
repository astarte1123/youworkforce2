#!/usr/bin/env bash
# ThingsBoard parity e2e: alarm policy, pull alarm nativi -> incidente, inventario,
# sync manuale, attributi, RPC, shared-attr, storico. GLOBEX (throwaway) vs TB :8084. MAI acme.
set -u
BASE=${BASE:-http://localhost:8080}
TB=${TB:-http://localhost:8084}; TB_USER=${TB_USER:-tenant@thingsboard.org}; TB_PASS=${TB_PASS:-tenant}
SLUG=globex; TID=77616511-8bc1-4bca-9ed5-9c78dee8cf27
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }

# TB: assicura un alarm attivo CRITICAL su un device (per testare il pull)
TBTOK=$(curl -s -X POST "$TB/api/auth/login" -H 'content-type: application/json' -d "{\"username\":\"$TB_USER\",\"password\":\"$TB_PASS\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
DID1=$(curl -s -H "X-Authorization: Bearer $TBTOK" "$TB/api/tenant/devices?pageSize=1&page=0" | python -c "import sys,json;print(json.load(sys.stdin)['data'][0]['id']['id'])")
curl -s -H "X-Authorization: Bearer $TBTOK" -X POST "$TB/api/alarm" -H 'content-type: application/json' -d "{\"originator\":{\"entityType\":\"DEVICE\",\"id\":\"$DID1\"},\"type\":\"QA High Temp\",\"severity\":\"CRITICAL\",\"status\":\"ACTIVE_UNACK\"}" >/dev/null

# superadmin: attiva modulo + connessione
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(csrf "$SJ"); sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
sapi -X POST "$BASE/api/superadmin/tenants/$TID/modules" -H 'content-type: application/json' -d '{"module_code":"thingsboard","state":"attivo"}' >/dev/null
sapi -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard" -H 'content-type: application/json' -d "{\"base_url\":\"$TB\",\"username\":\"$TB_USER\",\"password\":\"$TB_PASS\"}" >/dev/null

# tenant_admin globex
AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
AC=$(csrf "$AJ"); aapi(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" "$@"; }

# --- alarm policy ---
aapi -X POST "$BASE/api/thingsboard/alarm-policy" -H 'content-type: application/json' -d '{"severity":{"CRITICAL":"critico","MINOR":"ignora"}}' >/dev/null
GP=$(aapi "$BASE/api/thingsboard/alarm-policy")
[ "$(echo "$GP" | jv "d['severity']['CRITICAL']")" = "critico" ] && ok "alarm policy salvata (CRITICAL=critico)" || no "alarm policy: $GP"
[ "$(echo "$GP" | jv "d['severity']['MINOR']")" = "ignora" ] && ok "alarm policy (MINOR=ignora)" || no "policy minor"

# --- sync manuale (tenant admin) ---
SY=$(aapi -X POST "$BASE/api/thingsboard/sync")
echo "sync: $(echo "$SY"|head -c 120)"
[ "$(echo "$SY" | jv "d.get('ok')")" = "True" ] && ok "sync manuale (tenant admin)" || { no "sync: $SY"; echo "P:$PASS F:$FAIL"; exit 1; }
DEVC=$(echo "$SY" | jv "d['device_count']")
[ "${DEVC:-0}" -gt 0 ] && ok "sync specchia device ($DEVC)" || no "nessun device"

# --- inventario ---
INV=$(aapi "$BASE/api/thingsboard/devices")
DID=$(echo "$INV" | python -c "import sys,json;d=json.load(sys.stdin)['devices'];print(d[0]['id'] if d else '')" 2>/dev/null)
[ -n "$DID" ] && ok "inventario device TB" || { no "inventario vuoto"; echo "P:$PASS F:$FAIL"; exit 1; }

# --- pull alarm nativo -> incidente (CRITICAL=critico) ---
INC=$(docker exec ywdb psql -U postgres -d youworkforce -t -c "select count(*) from app.incidents i join app.tenants t on t.id=i.tenant_id where t.slug='globex' and i.etype='sensore_critico' and i.correlation like 'tb-alarm-%';" 2>/dev/null | tr -d ' \r\n')
[ "${INC:-0}" -ge 1 ] && ok "#1 alarm TB -> incidente sensore_critico ($INC)" || no "#1 alarm non ingerito (inc=$INC)"

# --- attributi ---
ATTR=$(aapi "$BASE/api/thingsboard/devices/$DID/attributes")
echo "$ATTR" | python -c "import sys,json;a=json.load(sys.stdin);sys.exit(0 if isinstance(a,list) else 1)" && ok "#2 attributi device (array)" || no "#2 attributi: $(echo "$ATTR"|head -c 100)"

# --- RPC (device offline -> ok:false ma risposta gestita) ---
RPC=$(aapi -X POST "$BASE/api/thingsboard/devices/$DID/rpc" -H 'content-type: application/json' -d '{"method":"ping","params":{},"oneway":true}')
echo "$RPC" | grep -q '"status"' && ok "#4 RPC gestito (status riportato)" || no "#4 RPC: $RPC"

# --- shared attributes (config remota) ---
SH=$(aapi -X POST "$BASE/api/thingsboard/devices/$DID/shared-attributes" -H 'content-type: application/json' -d '{"attributes":{"reportInterval":120}}')
[ "$(echo "$SH" | jv "d.get('ok')")" = "True" ] && ok "#5 shared attributes scritti" || no "#5 shared attr: $SH"

# --- storico ---
NOW=$(python -c "import time;print(int(time.time()*1000))")
FROM=$(python -c "import time;print(int(time.time()*1000)-7*86400000)")
HI=$(aapi "$BASE/api/thingsboard/devices/$DID/history?keys=temperature&from=$FROM&to=$NOW")
echo "$HI" | python -c "import sys,json;json.load(sys.stdin);print('ok')" >/dev/null 2>&1 && ok "#3 storico telemetria (json)" || no "#3 storico: $(echo "$HI"|head -c 100)"

echo "-------------------------------------"
echo "THINGSBOARD PARITY e2e: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
