#!/usr/bin/env bash
# End-to-end proof of the REAL Traccar badge source (UPSTREAM REALE):
#   device (OsmAnd protocol) -> Traccar 6.x (real) -> event forwarding
#     -> core adapter /api/ingress/traccar -> normalized SOS -> incident
#       -> outbox -> GoAlert (real, if GOALERT_BASE set) -> timeline.
#
# Prereqs: qa/run-traccar.sh has been run; core on :8080 (GOALERT_BASE set for the
# UPSTREAM REALE GoAlert step, otherwise that step is SIMULATO and still passes shape).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
OSMAND="${OSMAND:-http://localhost:5055}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }

J=$(mktemp)
curl -s -c "$J" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}' >/dev/null

incs(){ curl -s -b "$J" -H 'x-tenant-slug: acme' "$BASE/api/incidents"; }
# correlations of existing traccar incidents, before the new alarm
BEFORE=$(incs | python -c "import sys,json;print(' '.join(i['correlation'] for i in json.load(sys.stdin) if 'traccar-evt' in i['correlation']))")

TS=$(date +%s)
echo "== 1) send OsmAnd SOS to real Traccar =="
# Traccar emits an 'alarm' event only on a STATE CHANGE: send a clean position first
# (no alarm) so the following SOS is a transition that generates a fresh event.
curl -s -o /dev/null -m 5 "$OSMAND/?id=BADGE-001&lat=45.4668&lon=9.1905&timestamp=$((TS-2))"
sleep 2
code=$(curl -s -o /dev/null -w "%{http_code}" -m 5 "$OSMAND/?id=BADGE-001&lat=45.4668&lon=9.1905&timestamp=$TS&alarm=sos")
[ "$code" = "200" ] && ok "Traccar accepted OsmAnd position (200)" || no "OsmAnd http=$code"

echo "== 2) Traccar emits alarm event, forwards it, core opens a NEW incident =="
NEWCORR=""
for i in $(seq 1 20); do
  sleep 3
  CUR=$(incs | python -c "import sys,json;print(' '.join(i['correlation'] for i in json.load(sys.stdin) if 'traccar-evt' in i['correlation']))")
  for c in $CUR; do case " $BEFORE " in *" $c "*) : ;; *) NEWCORR="$c";; esac; done
  [ -n "$NEWCORR" ] && break
done
[ -n "$NEWCORR" ] && ok "new traccar incident created ($NEWCORR)" || { no "no new traccar incident"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

IID=$(incs | python -c "import sys,json;print([i['id'] for i in json.load(sys.stdin) if i['correlation']=='$NEWCORR'][0])")
INFO=$(incs | python -c "import sys,json;i=[x for x in json.load(sys.stdin) if x['correlation']=='$NEWCORR'][0];print(i['etype'],i['state'],i.get('site'))")
echo "   incident $IID -> $INFO"
echo "$INFO" | grep -q '^sos ' && ok "mapped to etype=sos (critical)" || no "wrong etype: $INFO"
echo "$INFO" | grep -q 'Milano' && ok "site inherited from device (Milano)" || no "site not inherited: $INFO"

echo "== 3) timeline shows notify from source=traccar and GoAlert ack =="
TL=""
for i in $(seq 1 6); do
  TL=$(curl -s -b "$J" -H 'x-tenant-slug: acme' "$BASE/api/incidents/$IID/timeline")
  echo "$TL" | grep -q goalert_sent && break; sleep 2
done
echo "$TL" | grep -q '"source": *"traccar"' || echo "$TL" | grep -q '"source":"traccar"' && ok "notify_requested carries source=traccar" || no "no source=traccar in timeline"
echo "$TL" | grep -q goalert_sent && ok "goalert_sent present" || no "no goalert_sent"
if echo "$TL" | grep -q 'UPSTREAM REALE'; then ok "GoAlert step profile: UPSTREAM REALE"; else
  echo "INFO  GoAlert step is SIMULATO (GOALERT_BASE not set) — set it for UPSTREAM REALE"; fi
echo "$TL" | grep -q "\"dedup\": *\"$NEWCORR\"" || echo "$TL" | grep -q "\"dedup\":\"$NEWCORR\"" && ok "GoAlert dedup carries the Traccar correlation" || no "dedup missing correlation"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
