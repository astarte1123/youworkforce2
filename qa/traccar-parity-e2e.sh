#!/usr/bin/env bash
# Traccar parity e2e: event policy (ignora/registra/critico), report proxy
# (trips/stops/summary), drivers/maintenance/computed CRUD+link, accumulators.
# Gira su GLOBEX (throwaway) contro Traccar vivo :8083. MAI acme.
set -u
BASE=${BASE:-http://localhost:8080}
TR=${TR:-http://localhost:8083}; TR_USER=${TR_USER:-op@acme.test}; TR_PASS=${TR_PASS:-Passw0rd!}
SLUG=globex; TID=77616511-8bc1-4bca-9ed5-9c78dee8cf27
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }

# superadmin: attiva modulo traccar + imposta connessione
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(csrf "$SJ"); sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
sapi -X POST "$BASE/api/superadmin/tenants/$TID/modules" -H 'content-type: application/json' -d '{"module_code":"traccar","state":"attivo"}' >/dev/null
sapi -X POST "$BASE/api/superadmin/tenants/$TID/traccar" -H 'content-type: application/json' -d "{\"base_url\":\"$TR\",\"username\":\"$TR_USER\",\"password\":\"$TR_PASS\"}" >/dev/null

# tenant_admin globex
AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
AC=$(csrf "$AJ"); aapi(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" "$@"; }

# --- event policy ---
aapi -X POST "$BASE/api/traccar/event-policy" -H 'content-type: application/json' -d '{"events":{"deviceOverspeed":"critico","deviceMoving":"ignora","unaccompaniedMotion":"critico"}}' >/dev/null
GP=$(aapi "$BASE/api/traccar/event-policy")
[ "$(echo "$GP" | jv "d['events']['deviceOverspeed']")" = "critico" ] && ok "event policy salvata (overspeed=critico)" || no "event policy: $GP"
[ "$(echo "$GP" | jv "d['events']['deviceMoving']")" = "ignora" ] && ok "event policy (moving=ignora)" || no "policy moving: $GP"

# device globex con external_id presente su Traccar (BADGE-001)
aapi -X POST "$BASE/api/devices" -H 'content-type: application/json' -d '{"external_id":"BADGE-001","source":"traccar","label":"Badge QA","enabled":true}' >/dev/null
DID=$(aapi "$BASE/api/devices" | python -c "import sys,json;print([d['id'] for d in json.load(sys.stdin) if d['external_id']=='BADGE-001'][0])" 2>/dev/null)
[ -n "$DID" ] && ok "device traccar creato" || { no "device non creato"; echo "P:$PASS F:$FAIL"; exit 1; }

# --- report proxy ---
FROM=$(python -c "import datetime;print((datetime.datetime.now(datetime.UTC)-datetime.timedelta(days=7)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TO=$(python -c "import datetime;print(datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TRIPS=$(aapi "$BASE/api/traccar/reports/trips?device_id=$DID&from=$FROM&to=$TO")
echo "$TRIPS" | python -c "import sys,json;a=json.load(sys.stdin);sys.exit(0 if isinstance(a,list) else 1)" && ok "report trips (array)" || no "report trips: $(echo "$TRIPS"|head -c 120)"
SUM=$(aapi "$BASE/api/traccar/reports/summary?device_id=$DID&from=$FROM&to=$TO")
echo "$SUM" | grep -q 'distance' && ok "report summary (distanza)" || no "report summary: $(echo "$SUM"|head -c 120)"
STOPS=$(aapi "$BASE/api/traccar/reports/stops?device_id=$DID&from=$FROM&to=$TO")
echo "$STOPS" | python -c "import sys,json;a=json.load(sys.stdin);sys.exit(0 if isinstance(a,list) else 1)" && ok "report stops (array)" || no "report stops"

# --- drivers ---
DRV=$(aapi -X POST "$BASE/api/traccar/drivers" -H 'content-type: application/json' -d "{\"name\":\"QA Driver\",\"unique_id\":\"QA-RFID-1\",\"device_id\":\"$DID\"}")
DRVID=$(echo "$DRV" | jv "d['id']")
[ -n "$DRVID" ] && ok "#6 driver creato+linkato" || no "#6 driver: $DRV"
aapi "$BASE/api/traccar/drivers" | grep -q 'QA-RFID-1' && ok "#6 driver elencato" || no "#6 driver non elencato"
[ -n "$DRVID" ] && aapi -X DELETE "$BASE/api/traccar/drivers/$DRVID" >/dev/null && ok "#6 driver eliminato" || no "#6 driver del"

# --- maintenance ---
MNT=$(aapi -X POST "$BASE/api/traccar/maintenance" -H 'content-type: application/json' -d "{\"name\":\"Tagliando QA\",\"type\":\"totalDistance\",\"start\":10000,\"period\":15000,\"device_id\":\"$DID\"}")
MNTID=$(echo "$MNT" | jv "d['id']")
[ -n "$MNTID" ] && ok "#7 maintenance creata+linkata" || no "#7 maintenance: $MNT"
aapi "$BASE/api/traccar/maintenance" | grep -q 'Tagliando QA' && ok "#7 maintenance elencata" || no "#7 maint non elencata"
[ -n "$MNTID" ] && aapi -X DELETE "$BASE/api/traccar/maintenance/$MNTID" >/dev/null && ok "#7 maintenance eliminata" || no "#7 maint del"

# --- computed attributes ---
CMP=$(aapi -X POST "$BASE/api/traccar/computed" -H 'content-type: application/json' -d "{\"description\":\"ignition\",\"attribute\":\"ignition\",\"expression\":\"power ? power > 13.2 : null\",\"type\":\"boolean\",\"device_id\":\"$DID\"}")
CMPID=$(echo "$CMP" | jv "d['id']")
[ -n "$CMPID" ] && ok "#8 computed attr creato+linkato" || no "#8 computed: $CMP"
aapi "$BASE/api/traccar/computed" | grep -q 'ignition' && ok "#8 computed elencato" || no "#8 computed non elencato"
[ -n "$CMPID" ] && aapi -X DELETE "$BASE/api/traccar/computed/$CMPID" >/dev/null && ok "#8 computed eliminato" || no "#8 computed del"

# --- accumulators ---
ACC=$(aapi "$BASE/api/traccar/accumulators/$DID")
echo "$ACC" | grep -q 'total_distance' && ok "#10 accumulators lettura" || no "#10 accumulators: $ACC"
AR=$(aapi -X POST "$BASE/api/traccar/accumulators/$DID" -H 'content-type: application/json' -d '{"total_distance":0,"hours":0}')
[ "$(echo "$AR" | jv "d.get('ok')")" = "True" ] && ok "#10 accumulators reset" || no "#10 accum reset: $AR"

# cleanup device QA (NON tocca acme)
aapi -X DELETE "$BASE/api/devices/$DID" >/dev/null
echo "cleanup device QA fatto"

echo "-------------------------------------"
echo "TRACCAR PARITY e2e: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
