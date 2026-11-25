#!/usr/bin/env bash
# Slice 1 — correttezza consuntivi: self-approval bloccato, versioning, arrotondamento half-up.
# Richiede core su :8080 con seed demo (acme) e TEST_CLOCK. Profilo SIMULATO.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
req(){ local m=$1 p=$2 jar=$3 data=$4
  curl -s -X "$m" "$BASE$p" -c "$jar" -b "$jar" -H 'content-type: application/json' \
    -H "x-tenant-slug: acme" -H "x-csrf-token: $(csrfh "$jar")" ${data:+-d "$data"}; }
code(){ local m=$1 p=$2 jar=$3 data=$4
  curl -s -o /dev/null -w '%{http_code}' -X "$m" "$BASE$p" -b "$jar" -H 'content-type: application/json' \
    -H "x-tenant-slug: acme" -H "x-csrf-token: $(csrfh "$jar")" ${data:+-d "$data"}; }

echo "== Consuntivi correttezza (Slice 1) =="

# login: mario (lavoratore, owner), responsabile (approve)
req POST /api/login "$J/m"  '{"email":"mario@acme.loneworker.eu","password":"demo"}' >/dev/null
req POST /api/login "$J/rs" '{"email":"responsabile@acme.loneworker.eu","password":"demo"}' >/dev/null

# 1) VERSIONING: create=1, dopo una riga=2
say "1 versioning incrementa (v2 dopo add_line)"
TS=$(req POST /api/timesheets "$J/m" '{}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST "/api/timesheets/$TS/lines" "$J/m" '{"activity":"x","work_date":"2026-11-10","minutes":15}' >/dev/null
req GET "/api/timesheets/$TS" "$J/m" | grep -q '"version":2' && ok || no "$(req GET /api/timesheets/$TS "$J/m" | grep -o '"version":[0-9]*')"

# 2) ARROTONDAMENTO half-up: 15 min @ 3350 cent/h -> 50250/60 = 837.5 -> 838 (non 837 troncato).
#    CA-14 (25/11/2026): la tariffa non la sceglie il lavoratore: la fissa chi approva in revisione.
say "2 arrotondamento half-up (838, non 837)"
req POST "/api/timesheets/$TS/submit" "$J/m" >/dev/null
LID=$(req GET "/api/timesheets/$TS" "$J/m" | python -c "import sys,json;print(json.load(sys.stdin)['lines'][0]['id'])")
req POST "/api/timesheets/$TS/lines/$LID/rate" "$J/rs" '{"rate_cents":3350}' >/dev/null
req GET "/api/timesheets/$TS" "$J/m" | grep -q '"total_cents":838' && ok || no "$(req GET /api/timesheets/$TS "$J/m" | grep -o '"total_cents":[0-9]*')"

# 3) SELF-APPROVAL bloccato: responsabile crea il PROPRIO consuntivo, invia, prova ad approvarlo -> 403
say "3 self-approval bloccato (403)"
TS2=$(req POST /api/timesheets "$J/rs" '{}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
req POST "/api/timesheets/$TS2/lines" "$J/rs" '{"activity":"y","work_date":"2026-11-10","minutes":60,"rate_cents":3000}' >/dev/null
req POST "/api/timesheets/$TS2/submit" "$J/rs" >/dev/null
c=$(code POST "/api/timesheets/$TS2/approve" "$J/rs")
[ "$c" = "403" ] && ok || no "got $c"

# 4) APPROVAZIONE da terzo con cap approve: responsabile approva il consuntivo di mario (diverso owner)
say "4 approvazione da approver diverso (ok)"
req POST "/api/timesheets/$TS/approve" "$J/rs" | grep -q approvato && ok || no "approve altrui"

# 5) POST-APPROVAZIONE immutabile: l'owner non può aggiungere righe a un approvato -> 409
say "5 approvato immutabile (409 su add_line)"
c=$(code POST "/api/timesheets/$TS/lines" "$J/m" '{"activity":"z","work_date":"2026-11-10","minutes":10,"rate_cents":100}')
[ "$c" = "409" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
