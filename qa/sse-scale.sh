#!/usr/bin/env bash
# Scalabilità eventi live: gli eventi passano per Postgres NOTIFY('yw_events') e vengono
# ribroadcastati nel bus SSE locale di OGNI istanza. Qui si prova la catena end-to-end
# su una istanza: SOS -> incidente -> NOTIFY -> LISTEN -> bus -> stream SSE del client.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }

J=$(mktemp); OUT=$(mktemp)
curl -s -o /dev/null -c "$J" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'

# apri lo stream SSE in background
curl -sN -b "$J" -H 'x-tenant-slug: acme' "$BASE/api/stream" > "$OUT" &
SSEPID=$!
sleep 2

CORR="sse-$(date +%s)-$RANDOM"
curl -s -o /dev/null -X POST "$BASE/api/events" -H 'content-type: application/json' -H 'x-ingest-token: acme-ingest-token' \
  -d "{\"etype\":\"sos\",\"source_event\":\"$CORR\",\"idem_key\":\"$CORR\",\"correlation\":\"$CORR\"}"

got=0
for i in $(seq 1 10); do sleep 1; grep -q "$CORR" "$OUT" && { got=1; break; }; done
kill "$SSEPID" 2>/dev/null || true

[ "$got" = "1" ] && ok "evento live ricevuto via SSE attraverso NOTIFY/LISTEN ($CORR)" || no "no SSE event" "$(tail -c 200 "$OUT")"
grep -q 'incident_opened' "$OUT" && ok "il payload SSE contiene incident_opened" || no "no incident_opened" "$(tail -c 200 "$OUT")"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
