#!/usr/bin/env bash
# Proof of REAL second-level notification DELIVERY (not just a local ack):
#   core SOS -> GoAlert (generic key) -> escalation policy -> on-call user
#     -> VERIFIED webhook contact method -> external receiver actually receives it.
#
# Transport = webhook to a test recipient (no real phone/emergency number), per the
# project's safety constraint. The on-call contact method is verified end-to-end by
# reading the verification code GoAlert delivers to that same receiver.
#
# Prereqs:
#   qa/run-goalert.sh         (GoAlert + oncall users on :8082)
#   docker run webhook receiver on network ywnet (started by this script if missing)
#   core on :8080 started with GOALERT_BASE=http://localhost:8082
# Requires: docker, python, curl.
set -uo pipefail
GA="${GA:-http://localhost:8082}"; GAU="${GA_USER:-admin}"; GAP="${GA_PASS:-goalertadmin1}"
CORE="${CORE:-http://localhost:8080}"
WH_URL="http://webhook:8080/"           # reachable by GoAlert over the ywnet docker network
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }

# 0) ensure the webhook receiver is up
if ! docker ps --format '{{.Names}}' | grep -q '^webhook$'; then
  docker run -d --name webhook --network ywnet -e HTTP_PORT=8080 mendhak/http-https-echo:latest >/dev/null
  sleep 3
fi
docker ps --format '{{.Names}}' | grep -q '^webhook$' && ok "webhook receiver running" || { no "receiver missing"; exit 1; }

J=$(mktemp -d)
curl -s -o /dev/null -c "$J/ga" -X POST "$GA/api/v2/identity/providers/basic" -H "Referer: $GA/" -H "Origin: $GA" \
  --data-urlencode "username=$GAU" --data-urlencode "password=$GAP"
gql(){ curl -s -b "$J/ga" -X POST "$GA/api/graphql" -H 'content-type: application/json' -H "Referer: $GA/" -d "$1"; }
gid(){ sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' <<<"$1" | head -1; }

# 1) enable webhook contact methods + allowlist (idempotent)
gql '{"query":"mutation{ setConfig(input:[{id:\"Webhook.Enable\",value:\"true\"},{id:\"Webhook.AllowedURLs\",value:\"http://webhook:8080\"}]) }"}' >/dev/null
ENABLED=$(gql '{"query":"{ config { id value } }"}' | python -c "import sys,json;print(next(c['value'] for c in json.load(sys.stdin)['data']['config'] if c['id']=='Webhook.Enable'))")
[ "$ENABLED" = "true" ] && ok "GoAlert webhook transport enabled" || no "webhook not enabled"

O1=$(gql '{"query":"{ users(input:{first:50}){ nodes{ id name } } }"}' | sed -n 's/.*"id":"\([0-9a-f-]*\)","name":"oncall1".*/\1/p')
[ -n "$O1" ] && ok "found on-call user oncall1" || { no "oncall1 missing (run qa/run-goalert.sh)"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

# 2) ensure a VERIFIED webhook contact method for oncall1 (reuse if present, else create)
SUF=$(date +%s)
CMID=$(gql "{\"query\":\"{ user(id:\\\"$O1\\\"){ contactMethods{ id type value disabled } } }\"}" \
  | python -c "import sys,json;cms=json.load(sys.stdin)['data']['user']['contactMethods'];print(next((c['id'] for c in cms if c['type']=='WEBHOOK' and c['value']=='$WH_URL'),''))")
if [ -n "$CMID" ]; then
  ok "reusing existing webhook contact method"
else
  CM=$(gql "{\"query\":\"mutation{ createUserContactMethod(input:{userID:\\\"$O1\\\",name:\\\"wh-$SUF\\\",type:WEBHOOK,value:\\\"$WH_URL\\\"}){ id } }\"}")
  CMID=$(gid "$CM")
  [ -n "$CMID" ] && ok "created webhook contact method" || { no "CM create failed: $CM"; echo "== $PASS passed, $FAIL failed =="; exit 1; }
fi
DIS=$(gql "{\"query\":\"{ userContactMethod(id:\\\"$CMID\\\"){ disabled } }\"}" | grep -o '"disabled":[a-z]*' | cut -d: -f2)
if [ "$DIS" != "false" ]; then
  gql "{\"query\":\"mutation{ sendContactMethodVerification(input:{contactMethodID:\\\"$CMID\\\"}) }\"}" >/dev/null
  CODE=""
  for i in $(seq 1 10); do
    sleep 2
    CODE=$(docker logs webhook --since 40s 2>&1 | grep -o '"Type":"Verification","Code":"[0-9]*"' | tail -1 | grep -o '[0-9]\{4,\}')
    [ -n "$CODE" ] && break
  done
  [ -n "$CODE" ] && ok "GoAlert delivered verification webhook (code received)" || no "no verification webhook received"
  gql "{\"query\":\"mutation{ verifyContactMethod(input:{contactMethodID:\\\"$CMID\\\",code:$CODE}) }\"}" >/dev/null
  DIS=$(gql "{\"query\":\"{ userContactMethod(id:\\\"$CMID\\\"){ disabled } }\"}" | grep -o '"disabled":[a-z]*' | cut -d: -f2)
fi
[ "$DIS" = "false" ] && ok "contact method verified (active)" || no "CM still disabled"
# immediate notification rule (ignore "already exists")
gql "{\"query\":\"mutation{ createUserNotificationRule(input:{userID:\\\"$O1\\\",contactMethodID:\\\"$CMID\\\",delayMinutes:0}){ id } }\"}" >/dev/null
ok "immediate notification rule ensured"

# 3) EP (step1 -> oncall1) + service + GENERIC integration key
EP=$(gid "$(gql "{\"query\":\"mutation{ createEscalationPolicy(input:{name:\\\"Delivery EP $SUF\\\",repeat:0,steps:[{delayMinutes:5,targets:[{type:user,id:\\\"$O1\\\"}]}]}){ id } }\"}")")
SID=$(gid "$(gql "{\"query\":\"mutation{ createService(input:{name:\\\"Delivery $SUF\\\",escalationPolicyID:\\\"$EP\\\"}){ id } }\"}")")
KEY=$(gid "$(gql "{\"query\":\"mutation{ createIntegrationKey(input:{serviceID:\\\"$SID\\\",type:generic,name:\\\"core-delivery\\\"}){ id } }\"}")")
[ -n "$KEY" ] && ok "service + generic integration key created" || no "key create failed"

# 4) point the acme tenant's GoAlert routing at this service (via core superadmin)
S=$(mktemp)
curl -s -c "$S" -X POST "$CORE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}' >/dev/null
SCSRF=$(awk '$6=="yw_csrf"{print $7}' "$S" 2>/dev/null | tail -1)
ACME=$(curl -s -b "$S" "$CORE/api/superadmin/tenants" | python -c "import sys,json;print([t['id'] for t in json.load(sys.stdin) if t['slug']=='acme'][0])")
curl -s -b "$S" -X POST "$CORE/api/superadmin/tenants/$ACME/goalert" -H 'content-type: application/json' -H "x-csrf-token: $SCSRF" \
  -d "{\"integration_key\":\"$KEY\"}" | grep -q '"ok":true' && ok "acme routed to the delivery service" || no "set goalert key failed"

# 5) fire a REAL core SOS and confirm GoAlert delivers it to the external receiver
CORR="delivery-$SUF"
curl -s -X POST "$CORE/api/events" -H 'content-type: application/json' -H 'x-ingest-token: acme-ingest-token' \
  -d "{\"etype\":\"sos\",\"source_event\":\"$CORR\",\"idem_key\":\"$CORR\",\"site_name\":\"Milano\",\"correlation\":\"$CORR\"}" | grep -q incident_id \
  && ok "core SOS ingested (incident opened)" || no "core SOS failed"

echo "waiting for GoAlert engine to deliver the alert to the webhook..."
GOT=""
for i in $(seq 1 20); do
  sleep 4
  GOT=$(docker logs webhook --since 120s 2>&1 | grep -F "$CORR")
  [ -n "$GOT" ] && break
done
if [ -n "$GOT" ]; then
  ok "REAL DELIVERY: GoAlert posted the alert (carrying $CORR) to the external receiver"
else
  no "alert not delivered to receiver within timeout"
fi

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
