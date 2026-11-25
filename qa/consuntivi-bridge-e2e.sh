#!/usr/bin/env bash
# Slice 2 — ponte intervento -> consuntivo: generazione bozza prefillata + idempotenza.
# Richiede core :8080 con seed demo (acme, moduli interventi+consuntivi attivi) e TEST_CLOCK.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
req(){ local m=$1 p=$2 jar=$3 data=$4
  curl -s -X "$m" "$BASE$p" -c "$jar" -b "$jar" -H 'content-type: application/json' \
    -H "x-tenant-slug: acme" -H "x-csrf-token: $(csrfh "$jar")" ${data:+-d "$data"}; }

echo "== Consuntivi: ponte intervento->consuntivo (Slice 2) =="
req POST /api/login "$J/pl" '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' >/dev/null
req POST /api/login "$J/m"  '{"email":"mario@acme.loneworker.eu","password":"demo"}' >/dev/null
MARIO=$(req GET /api/users "$J/pl" | sed -n 's/.*"email":"mario@acme.loneworker.eu","id":"\([^"]*\)".*/\1/p')

# ordine assegnato a mario con esecuzione (minuti + materiali)
OID=$(req POST /api/orders "$J/pl" '{"title":"Sostituzione cavo","site_name":"Milano"}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST "/api/orders/$OID/assign" "$J/pl" "{\"version\":1,\"user_id\":\"$MARIO\"}" >/dev/null
req POST "/api/orders/$OID/report" "$J/m" '{"work_minutes":120,"materials":[{"descr":"cavo","qta":2,"unita":"m"}],"outcome_kind":"completato"}' >/dev/null
# CA-18 (25/11/2026): il consuntivo si genera a intervento completato
ver(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT version FROM app.work_orders WHERE id='$OID'" | tr -d '[:space:]'; }
say "0 intervento non completato: niente consuntivo (409)"
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$OID/timesheet" -b "$J/m" -H "x-tenant-slug: acme" -H "x-csrf-token: $(csrfh "$J/m")")
[ "$c" = "409" ] && ok || no "got $c"
req POST "/api/orders/$OID/start" "$J/m" "{\"version\":$(ver)}" >/dev/null
req POST "/api/orders/$OID/complete" "$J/m" "{\"version\":$(ver),\"outcome\":\"ok\"}" >/dev/null

say "1 genera bozza consuntivo da ordine (created,line_added)"
R=$(req POST "/api/orders/$OID/timesheet" "$J/m")
echo "$R" | grep -q '"created":true' && echo "$R" | grep -q '"line_added":true' && ok || no "$R"
TS=$(echo "$R" | sed -n 's/.*"timesheet_id":"\([^"]*\)".*/\1/p')

say "2 bozza prefillata: 120 min, materiali in nota"
D=$(req GET "/api/timesheets/$TS" "$J/m")
echo "$D" | grep -q '"minutes":120' && echo "$D" | grep -q 'Materiali intervento' && ok || no "$D"

say "3 idempotenza: ri-genera non duplica (created:false,line_added:false)"
R2=$(req POST "/api/orders/$OID/timesheet" "$J/m")
echo "$R2" | grep -q '"created":false' && echo "$R2" | grep -q '"line_added":false' && \
  [ "$(echo "$R2" | sed -n 's/.*"timesheet_id":"\([^"]*\)".*/\1/p')" = "$TS" ] && ok || no "$R2"

say "4 una sola riga labor dopo doppia generazione"
n=$(req GET "/api/timesheets/$TS" "$J/m" | grep -o '"activity"' | wc -l | tr -d ' ')
[ "$n" = "1" ] && ok || no "righe=$n"

say "5 ordine non assegnato -> 400"
O2=$(req POST /api/orders "$J/pl" '{"title":"Senza assegnatario"}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST "/api/orders/$O2/report" "$J/pl" '{"work_minutes":30}' >/dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$O2/timesheet" -b "$J/pl" -H "x-tenant-slug: acme" -H "x-csrf-token: $(csrfh "$J/pl")")
[ "$c" = "400" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
