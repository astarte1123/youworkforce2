#!/usr/bin/env bash
# End-to-end smoke test against a running core (default :8080).
# Evidence profile: SIMULATO. Covers T01/T02/T03/T04/T05/T06 priority chains.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
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
R=$(req POST /api/events "$J/noauth" '{"etype":"sos","source_event":"SOS-1","device_external_id":"BADGE-001","site_name":"Milano","correlation":"corr-sos-1"}' 'x-ingest-token: acme-ingest-token')
echo "$R" | grep -q incident_id && ok || no "$R"
IID=$(echo "$R" | sed -n 's/.*"incident_id":"\([^"]*\)".*/\1/p')

# ---- T05 idempotency ----
say "T05 idempotent re-ingest (no 2nd incident)"
R=$(req POST /api/events "$J/noauth" '{"etype":"sos","source_event":"SOS-1","idem_key":"SOS-1","correlation":"corr-sos-1"}' 'x-ingest-token: acme-ingest-token')
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

echo "== $PASS passed, $FAIL failed =="
rm -rf "$J"; exit $FAIL
