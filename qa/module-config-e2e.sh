#!/usr/bin/env bash
# Per-module tool configuration (schema-driven):
#  - tenant_admin reads schema + effective config, saves values (validated)
#  - saved config actually drives behaviour (check-in clamp)
#  - superadmin availability gate hides a tool from the tenant
#  - RBAC: non-admin roles cannot configure
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
now(){ date +%s; }

JA=$(mktemp); JS=$(mktemp); JO=$(mktemp)

# --- logins ---
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | grep -o '"slug":"acme"[^}]*' >/dev/null && \
      curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c 'import sys,json;[print(t["id"]) for t in json.load(sys.stdin) if t["slug"]=="acme"]')

# 1) schema + defaults visible to tenant_admin
TOOLS=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/modules/loneworker/tools")
echo "$TOOLS" | grep -q 'checkin_timer' && ok "T1 tenant_admin vede strumento checkin_timer" || no "T1 no checkin_timer" "$TOOLS"
echo "$TOOLS" | grep -q '"default_minutes"' && ok "T2 schema+config esposti" || no "T2 no schema" "$TOOLS"

# 2) save valid config
CJ=$(csrfh "$JA")
R=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/loneworker/tools/checkin_timer" -d '{"config":{"default_minutes":5,"max_minutes":10}}')
echo "$R" | grep -q '"ok":true' && ok "T3 tenant_admin salva config" || no "T3 save failed" "$R"

# 3) behaviour: config drives the check-in timer. Compare due_at DIFFERENCES so the
#    result is immune to the controllable TEST_CLOCK offset left by other tests.
dueof(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/checkins" -d "{\"minutes\":$1}" | \
  python -c 'import sys,json,datetime;d=json.load(sys.stdin)["due_at"];print(int(datetime.datetime.fromisoformat(d.replace("Z","+00:00")).timestamp()))'; }
D_MAX=$(dueof 999)   # clamp -> max 10min
D_DEF=$(dueof 0)     # default 5min
D_ONE=$(dueof 1)     # explicit 1min (< max)
DIFF1=$((D_MAX - D_DEF))   # 10 - 5 = 5min = 300s
DIFF2=$((D_DEF - D_ONE))   # 5  - 1 = 4min = 240s
[ "$DIFF1" -ge 240 ] && [ "$DIFF1" -le 360 ] && ok "T4 clamp max 10 + default 5 (diff=${DIFF1}s~300)" || no "T4 clamp/default" "diff=$DIFF1"
[ "$DIFF2" -ge 180 ] && [ "$DIFF2" -le 300 ] && ok "T5 default 5 distinto da richiesta esplicita (diff=${DIFF2}s~240)" || no "T5 default" "diff=$DIFF2"

# 4) validation: out-of-range + unknown field rejected
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/loneworker/tools/checkin_timer" -d '{"config":{"default_minutes":99999}}')
[ "$C" = "400" ] && ok "T6 valore fuori range -> 400" || no "T6 range not enforced" "$C"
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/loneworker/tools/checkin_timer" -d '{"config":{"pippo":1}}')
[ "$C" = "400" ] && ok "T7 campo sconosciuto -> 400" || no "T7 unknown accepted" "$C"

# 5) RBAC: operatore cannot read/write config
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/modules/loneworker/tools")
[ "$C" = "403" ] && ok "T8 operatore non vede config (403)" || no "T8 operatore allowed" "$C"

# 6) superadmin availability gate: disable the tool, tenant stops seeing it
CS=$(csrfh "$JS")
curl -s -o /dev/null -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' \
  -X POST "$BASE/api/superadmin/tenants/$TID/tools" -d '{"module_code":"loneworker","tool_code":"checkin_timer","enabled":false}'
TOOLS=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/modules/loneworker/tools")
echo "$TOOLS" | grep -q 'checkin_timer' && no "T9 tool still visible after disable" "$TOOLS" || ok "T9 superadmin disabilita -> strumento nascosto al tenant"
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/loneworker/tools/checkin_timer" -d '{"config":{"default_minutes":5}}')
[ "$C" = "403" ] && ok "T10 config su strumento disabilitato -> 403" || no "T10 config allowed on disabled" "$C"

# cleanup: re-enable + restore defaults
curl -s -o /dev/null -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' \
  -X POST "$BASE/api/superadmin/tenants/$TID/tools" -d '{"module_code":"loneworker","tool_code":"checkin_timer","enabled":true}'
curl -s -o /dev/null -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CJ" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/loneworker/tools/checkin_timer" -d '{"config":{"default_minutes":30,"max_minutes":120}}'

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
