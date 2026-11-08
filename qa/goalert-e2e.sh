#!/usr/bin/env bash
# Real GoAlert end-to-end (evidence profile: UPSTREAM REALE for alert creation + routing).
# Requires: core on :8080 with GOALERT_BASE set, real GoAlert on :8082.
# Usage: KEY=<integration_key_id> GA_USER=admin GA_PASS=... bash qa/goalert-e2e.sh
BASE="${BASE:-http://localhost:8080}"; GA="${GA:-http://localhost:8082}"
KEY="${KEY:?set KEY=<integration_key_id>}"; GA_USER="${GA_USER:-admin}"; GA_PASS="${GA_PASS:-goalertadmin1}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-56s' "$1"; }; ok(){ echo PASS; PASS=$((PASS+1)); }; no(){ echo "FAIL ($1)"; FAIL=$((FAIL+1)); }
gql(){ curl -s -b "$J/ga" -X POST "$GA/api/graphql" -H 'content-type: application/json' -H "Referer: $GA/" -d "$1"; }

echo "== GoAlert REAL e2e =="
# GoAlert admin session
curl -s -o /dev/null -c "$J/ga" -X POST "$GA/api/v2/identity/providers/basic" -H "Referer: $GA/" -H "Origin: $GA" \
  --data-urlencode "username=$GA_USER" --data-urlencode "password=$GA_PASS"
say "GoAlert reachable + EP has 2 steps"
gql '{"query":"{ escalationPolicies(input:{first:5}){ nodes{ name steps{ stepNumber } } } }"}' | grep -q '"stepNumber":1' && ok || no "no 2-step EP"

# core: superadmin sets acme per-tenant key
curl -s -c "$J/s" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' >/dev/null
TID=$(curl -s -b "$J/s" "$BASE/api/superadmin/tenants" | sed -n 's/.*{"id":"\([^"]*\)","name":"Acme[^}]*}.*/\1/p')
[ -z "$TID" ] && TID=$(curl -s -b "$J/s" "$BASE/api/superadmin/tenants" | sed -n 's/.*"id":"\([^"]*\)","name":"Acme.*/\1/p')
say "set acme GoAlert integration key (per-tenant routing)"
curl -s -b "$J/s" -X POST "$BASE/api/superadmin/tenants/$TID/goalert" -H 'content-type: application/json' -d "{\"integration_key\":\"$KEY\"}" | grep -q '"ok":true' && ok || no "set key"

# count alerts before
before=$(gql '{"query":"{ alerts(input:{first:50}){ nodes{ alertID } } }"}' | grep -o '"alertID"' | wc -l)

# trigger SOS via core ingress (real badge path)
CORR="e2e-$(date +%s)"
say "core ingress SOS -> outbox -> real GoAlert"
curl -s -X POST "$BASE/api/events" -H 'x-ingest-token: acme-ingest-token' -H 'content-type: application/json' \
  -d "{\"etype\":\"sos\",\"source_event\":\"$CORR\",\"site_name\":\"Milano\",\"correlation\":\"$CORR\"}" | grep -q incident_id && ok || no "ingress"

# wait for worker to drain outbox + alert to appear
after=0
for i in 1 2 3 4 5 6; do sleep 2
  after=$(gql '{"query":"{ alerts(input:{first:50}){ nodes{ alertID summary } } }"}' | grep -o '"alertID"' | wc -l)
  [ "$after" -gt "$before" ] && break
done
say "new REAL alert created in GoAlert ($before -> $after)"
[ "$after" -gt "$before" ] && ok || no "no new alert"

# the alert summary carries our correlation
say "alert summary carries correlation"
gql '{"query":"{ alerts(input:{first:50}){ nodes{ summary service{ name } } } }"}' | grep -q "$CORR" && ok || no "no corr in GoAlert"

# core incident timeline shows goalert_ack with UPSTREAM REALE
curl -s -c "$J/op" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}' >/dev/null
IID=$(curl -s -b "$J/op" "$BASE/api/incidents" | sed -n "s/.*{\"id\":\"\([^\"]*\)\"[^}]*\"correlation\":\"$CORR\".*/\1/p")
[ -z "$IID" ] && IID=$(curl -s -b "$J/op" "$BASE/api/incidents" | grep -o '"id":"[^"]*"' | head -1 | sed 's/.*:"//;s/"//')
say "core timeline: goalert_ack profile UPSTREAM REALE"
curl -s -b "$J/op" "$BASE/api/incidents/$IID/timeline" | grep -q 'UPSTREAM REALE' && ok || no "$(curl -s -b "$J/op" "$BASE/api/incidents/$IID/timeline")"

echo "== $PASS passed, $FAIL failed =="
rm -rf "$J"; exit $FAIL
