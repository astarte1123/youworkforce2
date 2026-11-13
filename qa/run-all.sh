#!/usr/bin/env bash
# Runner di TUTTE le suite e2e contro lo stack in esecuzione, raggruppate per
# dipendenza. Usato in locale e in CI (dopo l'avvio dei servizi).
#   MODE=core    -> solo le suite che richiedono core+Postgres (gating affidabile)
#   MODE=engines -> solo le suite dei motori (GoAlert/Traccar/ThingsBoard), auto-skip se giù
#   MODE=all     -> tutto (default)
# RUN_SETUP=1    -> include anche le suite che richiedono setup esterno/stato acme fresco
#                   (goalert-e2e: KEY; goalert-delivery: receiver; goalert-p4/traccar-p1/
#                    traccar-conndown: dipendono da acme appena seedato + run-*.sh). Escluse di default.
# Uscita != 0 se almeno una suite eseguita fallisce. Ogni suite gira su globex/throwaway, MAI acme.
set -u
BASE=${BASE:-http://localhost:8080}
GA=${GOALERT_BASE:-http://localhost:8082}
TR=${TR:-http://localhost:8083}
TB=${TB:-http://localhost:8084}
MODE=${MODE:-all}; RUN_SETUP=${RUN_SETUP:-0}
PASS=0; FAIL=0; SKIP=0; FAILED_LIST=""

up(){ curl -s -o /dev/null -m 3 -w "%{http_code}" "$1" 2>/dev/null; }
run(){ local s=$1
  [ -f "qa/$s" ] || { echo "MISS  $s"; return; }
  if bash "qa/$s" >/tmp/ra_$s.log 2>&1; then echo "PASS  $s"; PASS=$((PASS+1));
  else echo "FAIL  $s   (tail: $(tail -1 /tmp/ra_$s.log | tr -d '\r' | cut -c1-80))"; FAIL=$((FAIL+1)); FAILED_LIST="$FAILED_LIST $s"; fi
}
skip(){ echo "SKIP  $1   ($2)"; SKIP=$((SKIP+1)); }

# ---- liste per tier ----
CORE="smoke.sh loneworker-e2e.sh acl-e2e.sh crypto-e2e.sh \
 auth-hardening.sh csrf-hardening.sh ratelimit-hardening.sh session-hardening.sh \
 module-gating-e2e.sh modularity-e2e.sh module-config-e2e.sh config-consumers-e2e.sh \
 geofence-e2e.sh floorplans-e2e.sh alarms-indoor-e2e.sh geocode-e2e.sh posfilter-e2e.sh \
 device-attrs-e2e.sh device-onboarding-e2e.sh sensor-rules-e2e.sh scenarios-e2e.sh \
 ai-e2e.sh ai-agent-e2e.sh reports-e2e.sh workforce-e2e.sh interventi-e2e.sh \
 iot-mgmt-e2e.sh mgmt-e2e.sh outbox-resilience-e2e.sh prompt8-e2e.sh sse-scale.sh \
 field-intervention-e2e.sh field-intervention-slice1-e2e.sh patrols-e2e.sh \
 user-groups-e2e.sh capabilities-e2e.sh custom-roles-e2e.sh teltonika-e2e.sh teltonika-gprs-e2e.sh"
GOALERT_OK="goalert-config-e2e.sh goalert-p123-e2e.sh goalert-parity-e2e.sh"
TRACCAR_OK="traccar-e2e.sh traccar-config-e2e.sh traccar-p2-e2e.sh traccar-poll-e2e.sh \
 traccar-sync-e2e.sh traccar-cmd-e2e.sh traccar-battery-policy-e2e.sh traccar-degraded-e2e.sh traccar-parity-e2e.sh"
TB_OK="thingsboard-conn-e2e.sh thingsboard-e2e.sh thingsboard-pull-e2e.sh thingsboard-parity-e2e.sh"
# suite che richiedono setup esterno o acme appena seedato (escluse se RUN_SETUP=0)
GOALERT_SETUP="goalert-e2e.sh goalert-delivery.sh goalert-p4-e2e.sh"
TRACCAR_SETUP="traccar-p1-e2e.sh traccar-conndown-e2e.sh"

[ "$(up "$BASE/health")" = "200" ] || { echo "core down ($BASE) — abort"; exit 2; }

if [ "$MODE" = "core" ] || [ "$MODE" = "all" ]; then
  echo "== CORE =="; for s in $CORE; do run "$s"; done
fi

if [ "$MODE" = "engines" ] || [ "$MODE" = "all" ]; then
  echo "== GOALERT =="
  if [ "$(up "$GA")" != "000" ]; then
    for s in $GOALERT_OK; do run "$s"; done
    [ "$RUN_SETUP" = "1" ] && for s in $GOALERT_SETUP; do run "$s"; done
  else for s in $GOALERT_OK; do skip "$s" "GoAlert $GA giù"; done; fi

  echo "== TRACCAR =="
  if [ "$(up "$TR/api/server")" != "000" ]; then
    for s in $TRACCAR_OK; do run "$s"; done
    [ "$RUN_SETUP" = "1" ] && for s in $TRACCAR_SETUP; do run "$s"; done
  else for s in $TRACCAR_OK; do skip "$s" "Traccar $TR giù"; done; fi

  echo "== THINGSBOARD =="
  if [ "$(up "$TB/api/auth/login")" != "000" ]; then
    for s in $TB_OK; do run "$s"; done
  else for s in $TB_OK; do skip "$s" "ThingsBoard $TB giù"; done; fi
fi

echo "=============================================="
echo "RUN-ALL [$MODE]: $PASS passed, $FAIL failed, $SKIP skipped"
[ "$FAIL" = "0" ] || echo "FALLITE:$FAILED_LIST"
[ "$FAIL" = "0" ]
