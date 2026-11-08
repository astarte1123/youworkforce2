#!/usr/bin/env bash
# End-to-end smoke test against a running core (default :8080).
# Evidence profile: SIMULATO. Covers T01/T02/T03/T04/T05/T06 priority chains.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
# Unique per-run keys so the SOS chain is reproducible against a persistent DB
# (fixed idem keys would be deduplicated on a re-run and fail the cascade).
RUN="${RUN:-$(date +%s)-$$-$RANDOM}"; SOS="SOS-$RUN"; CORR="corr-sos-$RUN"
say(){ printf '%-58s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
# curl helper: method path jar [data] [extra-header]
req(){ local m=$1 p=$2 jar=$3 data=$4 hdr=$5
  curl -s -X "$m" "$BASE$p" -c "$jar" -b "$jar" -H 'content-type: application/json' \
    ${hdr:+-H "$hdr"} ${data:+-d "$data"}; }

echo "== YouWorkforce smoke =="
say "T00 health";
[ "$(curl -s $BASE/health)" = '{"ok":true}' ] && ok || no "core down"

# ---- T01 control plane ----
say "T01 superadmin login"
req POST /api/superadmin/login "$J/s" '{"email":"admin@loneworker.eu","password":"admin"}' | grep -q superadmin && ok || no "login"
say "T01 list tenants has acme+globex"
T=$(req GET /api/superadmin/tenants "$J/s"); echo "$T" | grep -q acme && echo "$T" | grep -q globex && ok || no "$T"

# ---- logins ----
say "login acme operatore"
req POST /api/login "$J/op" '{"email":"operatore@acme.loneworker.eu","password":"demo"}' 'x-tenant-slug: acme' | grep -q '"ok":true' && ok || no "op"
say "login globex admin"
req POST /api/login "$J/gx" '{"email":"admin@globex.loneworker.eu","password":"demo"}' 'x-tenant-slug: globex' | grep -q '"ok":true' && ok || no "gx"
say "login mario lavoratore"
req POST /api/login "$J/m" '{"email":"mario@acme.loneworker.eu","password":"demo"}' 'x-tenant-slug: acme' | grep -q '"ok":true' && ok || no "mario"

# ---- T03 SOS chain (machine ingress) ----
say "T03 ingest SOS (acme badge)"
R=$(req POST /api/events "$J/noauth" "{\"etype\":\"sos\",\"source_event\":\"$SOS\",\"idem_key\":\"$SOS\",\"device_external_id\":\"BADGE-001\",\"site_name\":\"Milano\",\"correlation\":\"$CORR\"}" 'x-ingest-token: acme-ingest-token')
echo "$R" | grep -q incident_id && ok || no "$R"
IID=$(echo "$R" | sed -n 's/.*"incident_id":"\([^"]*\)".*/\1/p')

# ---- T05 idempotency ----
say "T05 idempotent re-ingest (no 2nd incident)"
R=$(req POST /api/events "$J/noauth" "{\"etype\":\"sos\",\"source_event\":\"$SOS\",\"idem_key\":\"$SOS\",\"correlation\":\"$CORR\"}" 'x-ingest-token: acme-ingest-token')
echo "$R" | grep -q '"deduplicated":true' && ok || no "$R"

# ---- T02 isolation ----
say "T02 acme sees its SOS"
req GET /api/incidents "$J/op" | grep -q '"sos"' && ok || no "acme blind"
say "T02 globex CANNOT see acme SOS"
G=$(req GET /api/incidents "$J/gx"); echo "$G" | grep -q "$IID" && no "LEAK: $G" || ok
say "T02 bad ingest token rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/events" -H 'x-ingest-token: WRONG' -H 'content-type: application/json' -d '{"etype":"sos","source_event":"x"}')
[ "$code" = "401" ] && ok || no "got $code"

# ---- T03 worker -> GoAlert ack in timeline ----
say "T03 wait worker -> goalert_ack (SIMULATO)"
for i in 1 2 3 4 5; do sleep 2; TL=$(req GET "/api/incidents/$IID/timeline" "$J/op"); echo "$TL" | grep -q goalert_ack && break; done
echo "$TL" | grep -q notify_requested && echo "$TL" | grep -q goalert_ack && ok || no "$TL"

# ---- ack + close ----
say "T03 operatore ack"
req POST "/api/incidents/$IID/ack" "$J/op" | grep -q '"ok":true' && ok || no "ack"
say "T03 operatore close"
req POST "/api/incidents/$IID/close" "$J/op" '{"outcome":"risolto test"}' | grep -q '"ok":true' && ok || no "close"
say "T02 auditor role cannot ack (RBAC)"
req POST /api/login "$J/aud" '{"email":"auditor@acme.loneworker.eu","password":"demo"}' 'x-tenant-slug: acme' >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/incidents/$IID/ack" -b "$J/aud")
[ "$code" = "403" ] || [ "$code" = "409" ] && ok || no "got $code"

# ---- T04 check-in timer via controllable clock ----
say "T04 create check-in + expire via clock"
CI=$(req POST /api/checkins "$J/m" '{"minutes":1}'); CID=$(echo "$CI" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST /api/_test/advance "$J/noauth" '{"seconds":120}' >/dev/null
MC=""; for i in 1 2 3 4 5; do sleep 2; MC=$(req GET /api/incidents "$J/op"); echo "$MC" | grep -q mancato_checkin && break; done
echo "$MC" | grep -q mancato_checkin && ok || no "no mancato_checkin"

# ---- worker-originated SOS ----
say "T03 worker PWA SOS (session)"
req POST /api/sos "$J/m" '{"lat":45.46,"lon":9.19}' | grep -q incident_id && ok || no "pwa sos"

# ---- Prompt 9: ordini + consuntivi (T14) ----
req POST /api/login "$J/pl" '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' 'x-tenant-slug: acme' >/dev/null
req POST /api/login "$J/rs" '{"email":"responsabile@acme.loneworker.eu","password":"demo"}' 'x-tenant-slug: acme' >/dev/null
MARIO=$(req GET /api/users "$J/pl" | sed -n 's/.*"email":"mario@acme.loneworker.eu","id":"\([^"]*\)".*/\1/p')

say "T14 planner creates order"
OID=$(req POST /api/orders "$J/pl" '{"title":"Manutenzione quadro","site_name":"Milano"}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
[ -n "$OID" ] && ok || no "no order id"
say "T14 lavoratore cannot create order (RBAC)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders" -b "$J/m" -H 'content-type: application/json' -d '{"title":"x"}')
[ "$code" = "403" ] && ok || no "got $code"
say "T14 assign order to mario"
req POST "/api/orders/$OID/assign" "$J/pl" "{\"version\":1,\"user_id\":\"$MARIO\"}" | grep -q assegnato && ok || no "assign"
say "T05 stale version rejected on assign"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orders/$OID/assign" -b "$J/pl" -H 'content-type: application/json' -d "{\"version\":1,\"user_id\":\"$MARIO\"}")
[ "$code" = "409" ] && ok || no "got $code"
say "T14 mario sees + starts order"
req GET /api/orders "$J/m" | grep -q "$OID" || no "mario blind"
req POST "/api/orders/$OID/start" "$J/m" '{"version":2}' | grep -q in_corso && ok || no "start"
say "T14 mario completes order"
req POST "/api/orders/$OID/complete" "$J/m" '{"version":3,"outcome":"ok sul posto"}' | grep -q completato && ok || no "complete"
say "T02 globex cannot see acme order"
req GET /api/orders "$J/gx" | grep -q "$OID" && no "LEAK" || ok

say "T14 mario creates timesheet + line + expense"
TS=$(req POST /api/timesheets "$J/m" "{\"work_order_id\":\"$OID\"}" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST "/api/timesheets/$TS/lines" "$J/m" '{"activity":"riparazione","work_date":"2026-11-08","minutes":90,"rate_cents":3500}' | grep -q ok && \
req POST "/api/timesheets/$TS/expenses" "$J/m" '{"description":"ricambi","amount_cents":1250,"has_receipt":true}' | grep -q ok && ok || no "lines"
say "T13 deterministic total (90m@35e/h + 12.50 = 65.00)"
req GET "/api/timesheets/$TS" "$J/m" | grep -q '"total_cents":6500' && ok || no "$(req GET /api/timesheets/$TS "$J/m")"
say "T14 submit + approve"
req POST "/api/timesheets/$TS/submit" "$J/m" | grep -q in_revisione && \
req POST "/api/timesheets/$TS/approve" "$J/rs" | grep -q approvato && ok || no "approve"
say "T14 double approval blocked"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/timesheets/$TS/approve" -b "$J/rs")
[ "$code" = "409" ] && ok || no "got $code"
say "T14 CSV export has total row"
curl -s "$BASE/api/timesheets/$TS/export.csv" -b "$J/m" | grep -q "TOTALE" && ok || no "csv"

# ---- Prompt 7: assistente AI (T11) ----
say "T11 AI read query returns source+provider"
R=$(req POST /api/assistant "$J/op" '{"q":"ricostruisci l'\''ultimo allarme"}')
echo "$R" | grep -q '"provider":"SIMULATO"' && echo "$R" | grep -q '"intent":"incidente_storia"' && ok || no "$R"
say "T11 AI refuses critical action (close SOS)"
req POST /api/assistant "$J/op" '{"q":"chiudi il SOS adesso"}' | grep -q '"intent":"critical_refused"' && ok || no "no refusal"
say "T11 AI checkin query cites period"
req POST /api/assistant "$J/op" '{"q":"check-in scaduti al sito Milano"}' | grep -q '"intent":"checkins_scaduti"' && ok || no "intent"
say "T11 AI gated: globex (module off) -> 403"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/assistant" -b "$J/gx" -H 'content-type: application/json' -d '{"q":"ordini"}')
[ "$code" = "403" ] && ok || no "got $code"
say "T11 AI unknown -> clarify (no guess)"
req POST /api/assistant "$J/op" '{"q":"xyzzy qwerty"}' | grep -q '"intent":"clarify"' && ok || no "no clarify"

echo "== $PASS passed, $FAIL failed =="
rm -rf "$J"; exit $FAIL
