#!/usr/bin/env bash
# Config actually drives behaviour across modules:
#  - loneworker/sos_escalation -> incident priority (alta=3 / media=2)
#  - consuntivi/rates          -> billed-minutes rounding + default currency
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }

JA=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CM=$(csrfh "$JM")
setcfg(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/$1/tools/$2" -d "$3" >/dev/null; }
prio_of(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/incidents" | \
  python -c "import sys,json;print(next((i['priority'] for i in json.load(sys.stdin) if i['id']=='$1'),'?'))"; }
sos(){ curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' \
  -X POST "$BASE/api/sos" -d '{"lat":45.4,"lon":9.1}' | python -c 'import sys,json;print(json.load(sys.stdin)["incident_id"])'; }

# --- sos_escalation ---
setcfg loneworker sos_escalation '{"config":{"priority":"media","repeat_seconds":60}}'
I1=$(sos); P1=$(prio_of "$I1")
[ "$P1" = "2" ] && ok "sos_escalation media -> incidente priorita 2 ($I1)" || no "priority not media" "$P1"
setcfg loneworker sos_escalation '{"config":{"priority":"alta","repeat_seconds":0}}'
I2=$(sos); P2=$(prio_of "$I2")
[ "$P2" = "3" ] && ok "sos_escalation alta -> incidente priorita 3 ($I2)" || no "priority not alta" "$P2"

# --- consuntivi/rates: round 30min, currency USD ---
setcfg consuntivi rates '{"config":{"round_minutes":30,"currency":"USD"}}'
TS=$(curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' \
  -X POST "$BASE/api/timesheets" -d '{}' | python -c 'import sys,json;print(json.load(sys.stdin)["id"])')
# 10 real minutes @ 6000 cent/h ; rounded up to 30 -> labor = 30*6000/60 = 3000
curl -s -o /dev/null -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' \
  -X POST "$BASE/api/timesheets/$TS/lines" -d '{"activity":"x","work_date":"2026-11-08","minutes":10,"rate_cents":6000}'
D=$(curl -s -b "$JM" -H 'x-tenant-slug: acme' "$BASE/api/timesheets/$TS")
echo "$D" | python -c 'import sys,json;d=json.load(sys.stdin);t=d["total"];assert t["billed_minutes"]==30,t;assert t["labor_cents"]==3000,t;assert d["currency"]=="USD",d;print("ok")' >/dev/null 2>&1 \
  && ok "rates round 30 -> 10min fatturati 30, labor 3000, valuta USD" || no "rounding/currency wrong" "$(echo "$D" | head -c 200)"
# line inherited default currency USD
echo "$D" | grep -q '"currency":"USD"' && ok "riga eredita valuta di default USD" || no "line currency" "$D"

# --- workforce/shift: max_hours surfaced on turno ---
mpost(){ curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' "$@"; }
mpost -X POST "$BASE/api/shifts/end" >/dev/null   # ensure no open shift
setcfg workforce shift '{"config":{"max_hours":8}}'
mpost -X POST "$BASE/api/shifts/start" -d '{}' >/dev/null
CUR=$(curl -s -b "$JM" -H 'x-tenant-slug: acme' "$BASE/api/shifts/current")
echo "$CUR" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["active"],d;assert d["max_hours"]==8,d;assert d["over_max"]==False,d;print("ok")' >/dev/null 2>&1 \
  && ok "shift/max_hours=8 esposto su turno in corso, over_max false" || no "shift max_hours" "$(echo "$CUR"|head -c 160)"
END=$(mpost -X POST "$BASE/api/shifts/end")
echo "$END" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["max_hours"]==8,d;assert "worked_minutes" in d,d;print("ok")' >/dev/null 2>&1 \
  && ok "shift_end riporta worked_minutes + max_hours" || no "shift_end fields" "$(echo "$END"|head -c 160)"

# restore defaults
setcfg consuntivi rates '{"config":{"round_minutes":15,"currency":"EUR"}}'
setcfg workforce shift '{"config":{"max_hours":12}}'

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
