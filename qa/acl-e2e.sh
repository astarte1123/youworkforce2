#!/usr/bin/env bash
# #8 ACL per-sede: operatore ristretto vede solo i device della/e sua/e sede/i.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"
JA=$(mktemp); JO=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA");
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
O(){ curl -s -b "$JO" -H 'x-tenant-slug: acme' "$@"; }
OPID=$(A "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("operatore@")))')

SA=$(A -X POST "$BASE/api/sites" -d "{\"name\":\"SedeA $RUN\"}" | jget 'd["id"]')
SB=$(A -X POST "$BASE/api/sites" -d "{\"name\":\"SedeB $RUN\"}" | jget 'd["id"]')
DA=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"ACL-A-$RUN\",\"source\":\"manual\",\"site_id\":\"$SA\"}" | jget 'd["id"]')
DB=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"ACL-B-$RUN\",\"source\":\"manual\",\"site_id\":\"$SB\"}" | jget 'd["id"]')

cnt(){ O "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);print(sum(1 for x in d if x['external_id'] in ('ACL-A-$RUN','ACL-B-$RUN')))"; }
[ "$(cnt)" = "2" ] && ok "operatore senza restrizioni vede entrambi i device" || no "unrestricted" "$(cnt)"

# restringi operatore a SedeA
A -X POST "$BASE/api/users/$OPID/sites" -d "{\"site_ids\":[\"$SA\"]}" | grep -q '"ok":true' && ok "sedi operatore impostate (SedeA)" || no "set sites" "-"
A "$BASE/api/users/$OPID/sites" | grep -q "$SA" && ok "sedi operatore leggibili" || no "get sites" "-"
[ "$(cnt)" = "1" ] && O "$BASE/api/devices" | grep -q "ACL-A-$RUN" && O "$BASE/api/devices" | grep -qv "ACL-B-$RUN" && ok "operatore ristretto vede solo device di SedeA" || no "restricted" "$(cnt)"
# admin vede sempre entrambi
[ "$(A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);print(sum(1 for x in d if x['external_id'] in ('ACL-A-$RUN','ACL-B-$RUN')))")" = "2" ] && ok "admin vede entrambi (non ristretto)" || no "admin all" "-"

# rimuovi restrizione
A -X POST "$BASE/api/users/$OPID/sites" -d '{"site_ids":[]}' >/dev/null
[ "$(cnt)" = "2" ] && ok "rimossa restrizione -> operatore rivede entrambi" || no "reset" "$(cnt)"

# cleanup
A -X DELETE "$BASE/api/devices/$DA" >/dev/null; A -X DELETE "$BASE/api/devices/$DB" >/dev/null
A -X DELETE "$BASE/api/sites/$SA" >/dev/null; A -X DELETE "$BASE/api/sites/$SB" >/dev/null
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
