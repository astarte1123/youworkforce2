#!/usr/bin/env bash
# Gap #1 — Attribuzione temporale (C08): un evento è attribuito al lavoratore valido ALL'ORARIO
# dell'evento (origin_ts), non all'assegnatario corrente; marcatura di incertezza se l'orario
# non è affidabile e il device ha cambiato assegnatario.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
R=$RANDOM
TID=$(one "SELECT id FROM app.tenants WHERE slug='globex'")
UA=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','attrA-$R@globex.loneworker.eu','x','operatore') RETURNING id")
UB=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','attrB-$R@globex.loneworker.eu','x','operatore') RETURNING id")
DID=$(one "INSERT INTO app.devices(tenant_id,source,external_id,label,assigned_user,enabled) VALUES('$TID','traccar','TATTR-$R','Badge attr','$UB'::uuid,true) RETURNING id")
TOK="tattr$R"; sql "INSERT INTO app.ingest_tokens(tenant_id,token,source,device_id) VALUES('$TID','$TOK','traccar','$DID')" >/dev/null
# Storico: A [now-3d .. now-1d], B [now-1d .. aperta]; assegnatario corrente = B
sql "INSERT INTO app.device_assignments(tenant_id,device_id,user_id,assigned_at,unassigned_at) VALUES('$TID','$DID','$UA',now()-interval '3 days',now()-interval '1 day')" >/dev/null
sql "INSERT INTO app.device_assignments(tenant_id,device_id,user_id,assigned_at) VALUES('$TID','$DID','$UB',now()-interval '1 day')" >/dev/null

POST(){ curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' -d "$1"; }
# worker + incertezza dell'ULTIMO incidente del device con un dato etype-id (via events.device_id)
W(){ sql "SELECT COALESCE(u.email,'-')||'|'||i.attribution_uncertain FROM app.incidents i
     JOIN app.events e ON e.id=i.event_id LEFT JOIN app.users u ON u.id=i.worker_user_id
     WHERE e.device_id='$DID' AND e.source_event LIKE '%$1%' ORDER BY i.opened_at DESC LIMIT 1"; }
DTA=$(python -c "import datetime;print((datetime.datetime.now(datetime.UTC)-datetime.timedelta(days=2)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
DTB=$(python -c "import datetime;print((datetime.datetime.now(datetime.UTC)-datetime.timedelta(hours=2)).strftime('%Y-%m-%dT%H:%M:%SZ'))")

cA=$(POST "{\"device\":{\"uniqueId\":\"TATTR-$R\"},\"event\":{\"id\":\"A$R\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"position\":{\"deviceTime\":\"$DTA\",\"latitude\":45,\"longitude\":9}}")
cB=$(POST "{\"device\":{\"uniqueId\":\"TATTR-$R\"},\"event\":{\"id\":\"B$R\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"position\":{\"deviceTime\":\"$DTB\",\"latitude\":45,\"longitude\":9}}")
cN=$(POST "{\"device\":{\"uniqueId\":\"TATTR-$R\"},\"event\":{\"id\":\"N$R\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"position\":{\"latitude\":45,\"longitude\":9}}")
{ [ "$cA" = 200 ] && [ "$cB" = 200 ] && [ "$cN" = 200 ]; } && ok "ingress 3 SOS accettati" || no "ingress ko" "$cA/$cB/$cN"
sleep 1
rA=$(W "A$R"); rB=$(W "B$R"); rN=$(W "N$R")
echo "$rA" | grep -q "attrA-$R.*|f" && ok "evento in finestra A -> attribuito ad A (certo)" || no "A-window ko" "$rA"
echo "$rB" | grep -q "attrB-$R.*|t\?" && ok "evento in finestra B -> attribuito a B" || no "B-window ko" "$rB"
echo "$rN" | grep -q "attrB-$R.*|t" && ok "evento senza orario + più assegnatari -> B ma INCERTO" || no "no-time ko" "$rN"

# teardown
sql "DELETE FROM app.incidents WHERE event_id IN (SELECT id FROM app.events WHERE device_id='$DID')" >/dev/null
sql "DELETE FROM app.events WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.device_assignments WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.devices WHERE id='$DID'" >/dev/null
sql "DELETE FROM app.users WHERE id IN ('$UA','$UB')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
