#!/usr/bin/env bash
# Configure real GoAlert via GraphQL: 2-step escalation policy (oncall1 -> oncall2),
# a service bound to it, and a GENERIC integration key. Prints KEY=<token>.
# Feed that token to the core: superadmin -> /api/superadmin/tenants/{id}/goalert.
set -e
GA="${GA:-http://localhost:8082}"; U="${GA_USER:-admin}"; P="${GA_PASS:-goalertadmin1}"
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/ga" -X POST "$GA/api/v2/identity/providers/basic" -H "Referer: $GA/" -H "Origin: $GA" \
  --data-urlencode "username=$U" --data-urlencode "password=$P"
gql(){ curl -s -b "$J/ga" -X POST "$GA/api/graphql" -H 'content-type: application/json' -H "Referer: $GA/" -d "$1"; }
getid(){ sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' <<<"$1" | head -1; }

USERS=$(gql '{"query":"{ users(input:{first:50}){ nodes{ id name } } }"}')
O1=$(sed -n 's/.*"id":"\([0-9a-f-]*\)","name":"oncall1".*/\1/p' <<<"$USERS")
O2=$(sed -n 's/.*"id":"\([0-9a-f-]*\)","name":"oncall2".*/\1/p' <<<"$USERS")
[ -z "$O1" ] || [ -z "$O2" ] && { echo "oncall users missing — run qa/run-goalert.sh first"; exit 1; }

SUF=$(date +%s)
EP=$(getid "$(gql "{\"query\":\"mutation{ createEscalationPolicy(input:{name:\\\"Acme LoneWorker EP $SUF\\\",repeat:1,steps:[{delayMinutes:1,targets:[{type:user,id:\\\"$O1\\\"}]},{delayMinutes:1,targets:[{type:user,id:\\\"$O2\\\"}]}]}){ id } }\"}")")
SID=$(getid "$(gql "{\"query\":\"mutation{ createService(input:{name:\\\"Acme LoneWorker $SUF\\\",escalationPolicyID:\\\"$EP\\\"}){ id } }\"}")")
IK=$(gql "{\"query\":\"mutation{ createIntegrationKey(input:{serviceID:\\\"$SID\\\",type:generic,name:\\\"core-outbox\\\"}){ id } }\"}")
KEY=$(getid "$IK")
rm -rf "$J"
echo "EP=$EP SERVICE=$SID"
echo "KEY=$KEY"
