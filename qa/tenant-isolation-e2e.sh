#!/usr/bin/env bash
# Isolamento multi-tenant degli allarmi: una chiusura (singola o bulk) in un tenant NON deve
# toccare gli allarmi di un altro tenant. Richiede core :8080 con seed demo + TEST_CLOCK.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
req(){ local m=$1 p=$2 jar=$3 data=$4 slug=$5
  curl -s -X "$m" "$BASE$p" -c "$jar" -b "$jar" -H 'content-type: application/json' \
    ${slug:+-H "x-tenant-slug: $slug"} -H "x-csrf-token: $(csrfh "$jar")" ${data:+-d "$data"}; }
RUN=$(date +%s)-$$

echo "== Isolamento multi-tenant allarmi =="

# superadmin: abilita loneworker su globex (per dargli allarmi reali)
req POST /api/superadmin/login "$J/sa" '{"email":"admin@loneworker.eu","password":"admin"}' >/dev/null
GID=$(req GET /api/superadmin/tenants "$J/sa" | python -c "import sys,json;print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='globex'),''))" 2>/dev/null)
req POST "/api/superadmin/tenants/$GID/modules" "$J/sa" '{"module_code":"loneworker","state":"attivo"}' >/dev/null

# incidente APERTO su ACME
AMD="ISO-A-$RUN"
curl -s -X POST $BASE/api/events -H 'content-type: application/json' -H 'x-ingest-token: acme-ingest-token' \
  -d "{\"etype\":\"man_down\",\"source_event\":\"$AMD\",\"idem_key\":\"$AMD\",\"correlation\":\"$AMD\"}" -o /dev/null
# incidente APERTO su GLOBEX
GMD="ISO-G-$RUN"
curl -s -X POST $BASE/api/events -H 'content-type: application/json' -H 'x-ingest-token: globex-ingest-token' \
  -d "{\"etype\":\"man_down\",\"source_event\":\"$GMD\",\"idem_key\":\"$GMD\",\"correlation\":\"$GMD\"}" -o /dev/null

req POST /api/login "$J/a" '{"email":"admin@acme.loneworker.eu","password":"demo"}' acme >/dev/null
req POST /api/login "$J/g" '{"email":"admin@globex.loneworker.eu","password":"demo"}' globex >/dev/null

acme_open(){ req GET /api/incidents "$J/a" '' acme | grep -o '"state":"[^"]*"' | grep -vc '"chiuso"'; }
glob_open(){ req GET /api/incidents "$J/g" '' globex | grep -o '"state":"[^"]*"' | grep -vc '"chiuso"'; }

A0=$(acme_open); G0=$(glob_open)
say "setup: acme e globex hanno allarmi aperti"
[ "$A0" -ge 1 ] && [ "$G0" -ge 1 ] && ok || no "acme=$A0 globex=$G0"

# GLOBEX chiude tutti i propri
R=$(req POST /api/incidents/close-all "$J/g" '{"outcome":"iso test globex"}' globex)
say "globex close-all chiude i SUOI"
echo "$R" | grep -q '"closed"' && [ "$(glob_open)" = "0" ] && ok || no "$R globex_dopo=$(glob_open)"

say "ACME intatto dopo close-all di globex (nessun leak)"
A1=$(acme_open); [ "$A1" = "$A0" ] && [ "$A1" -ge 1 ] && ok || no "acme prima=$A0 dopo=$A1"

# viceversa: globex non vede/chiude incidenti acme per id
AID=$(req GET /api/incidents "$J/a" '' acme | python -c "import sys,json;print(next((i['id'] for i in json.load(sys.stdin) if i['state']!='chiuso'),''))" 2>/dev/null)
say "globex close singolo di incidente acme -> bloccato"
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/g" -H 'x-tenant-slug: globex' -H "x-csrf-token: $(csrfh "$J/g")" -X POST "$BASE/api/incidents/$AID/close" -H 'content-type: application/json' -d '{"outcome":"x"}')
[ "$c" != "200" ] && [ "$(acme_open)" = "$A0" ] && ok || no "code=$c acme=$(acme_open)"

# cleanup: chiudi acme + rimetti globex loneworker non_abilitato
req POST /api/incidents/close-all "$J/a" '{"outcome":"cleanup"}' acme >/dev/null
req POST "/api/superadmin/tenants/$GID/modules" "$J/sa" '{"module_code":"loneworker","state":"non_abilitato"}' >/dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
