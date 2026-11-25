#!/usr/bin/env bash
# Runner di TUTTE le suite e2e contro lo stack in esecuzione, raggruppate per
# dipendenza. Usato in locale e in CI (dopo l'avvio dei servizi).
#   MODE=core|all -> tutte le suite: richiedono solo core + Postgres (YouWorkforce è self-contained,
#                    nessun motore esterno da avviare). MODE resta per compatibilità con la CI.
# Uscita != 0 se almeno una suite eseguita fallisce. Ogni suite gira su globex/throwaway, MAI acme.
set -u
BASE=${BASE:-http://localhost:8080}
MODE=${MODE:-all}
PASS=0; FAIL=0; SKIP=0; FAILED_LIST=""

up(){ curl -s -o /dev/null -m 3 -w "%{http_code}" "$1" 2>/dev/null; }
run(){ local s=$1
  [ -f "qa/$s" ] || { echo "MISS  $s"; return; }
  if bash "qa/$s" >/tmp/ra_$s.log 2>&1; then echo "PASS  $s"; PASS=$((PASS+1));
  else echo "FAIL  $s   (tail: $(tail -1 /tmp/ra_$s.log | tr -d '\r' | cut -c1-80))"; FAIL=$((FAIL+1)); FAILED_LIST="$FAILED_LIST $s"; fi
}
skip(){ echo "SKIP  $1   ($2)"; SKIP=$((SKIP+1)); }

# ---- liste per tier ----
CORE="console-syntax.sh smoke.sh loneworker-e2e.sh checkin-sos-attribution-e2e.sh immobility-e2e.sh acl-e2e.sh \
 auth-hardening.sh csrf-hardening.sh ratelimit-hardening.sh session-hardening.sh \
 module-gating-e2e.sh modularity-e2e.sh module-config-e2e.sh config-consumers-e2e.sh \
 geofence-e2e.sh floorplans-e2e.sh alarms-indoor-e2e.sh geofence-alarm-e2e.sh geocode-e2e.sh posfilter-e2e.sh \
 device-attrs-e2e.sh device-onboarding-e2e.sh device-onboarding-kit-e2e.sh sensor-rules-e2e.sh sensor-rule-scope-e2e.sh scenarios-e2e.sh \
 ai-e2e.sh ai-agent-e2e.sh reports-e2e.sh workforce-e2e.sh interventi-e2e.sh \
 iot-mgmt-e2e.sh mgmt-e2e.sh prompt8-e2e.sh sse-scale.sh \
 field-intervention-e2e.sh field-intervention-slice1-e2e.sh patrols-e2e.sh field-integrity-e2e.sh patrol-missed-e2e.sh \
 user-groups-e2e.sh capabilities-e2e.sh custom-roles-e2e.sh iam-guardrails-e2e.sh capabilities-granular-e2e.sh capabilities-admin-split-e2e.sh gdpr-audit-e2e.sh gdpr-anonymize-full-e2e.sh field-group-e2e.sh field-permissions-e2e.sh timesheet-corrections-e2e.sh state-guards-e2e.sh stale-open-e2e.sh module-autonomy-e2e.sh dashboard-layout-e2e.sh teltonika-e2e.sh teltonika-gprs-e2e.sh device-attempts-e2e.sh silence-shift-e2e.sh gh5200-lifesafety-e2e.sh telt-retransmit-e2e.sh gps-native-e2e.sh gps-isolation-e2e.sh gps-service-watchdog-e2e.sh gps-standalone-e2e.sh gps-battery-policy-e2e.sh osmand-ingest-e2e.sh crypto-e2e.sh setup-status-e2e.sh home-tasks-e2e.sh \
 cantieri-e2e.sh cantieri-squadre-e2e.sh cantieri-cronoprogramma-e2e.sh cantieri-costi-e2e.sh cantieri-giornale-fatture-e2e.sh cronoprogramma-deps-e2e.sh \
 clients-e2e.sh listino-e2e.sh dispatch-e2e.sh richieste-e2e.sh contratti-e2e.sh anagrafiche-e2e.sh commessa-allegati-e2e.sh tenant-invite-e2e.sh tenant-lifecycle-e2e.sh tenant-suspension-e2e.sh superadmin-hardening-e2e.sh license-advisory-e2e.sh system-monitoring-e2e.sh \
 vigilanza-e2e.sh vigilanza-piani-e2e.sh vigilanza-autopunch-e2e.sh \
 escalation-decouple-e2e.sh escalation-fallback-e2e.sh miei-allarmi-e2e.sh sms-ack-e2e.sh escalation-setactive-e2e.sh escalation-rest-e2e.sh escalation-schedules-e2e.sh \
 escalation-policy-e2e.sh escalation-policy-targets-e2e.sh escalation-profile-e2e.sh escalation-services-e2e.sh escalation-schedule-minors-e2e.sh ywalert-delivery-e2e.sh \
 iot-telemetry-e2e.sh iot-attr-cmd-e2e.sh iot-threshold-e2e.sh iot-normalize-e2e.sh iot-blueprint-e2e.sh iot-mqtt-e2e.sh iot-alerts-e2e.sh iot-monitor-e2e.sh iot-catalog-e2e.sh"
# NB: motore di escalation YWAlert = NATIVO (nessun connettore esterno). Le suite 'escalation-*' +
# 'ywalert-delivery' coprono la parità; le vecchie suite 'goalert-*' (prodotto esterno) sono rimosse.
[ "$(up "$BASE/health")" = "200" ] || { echo "core down ($BASE) — abort"; exit 2; }

# Sink webhook condiviso per la prova di recapito reale YWAlert (ywalert-delivery-e2e). Se core è
# avviato con YW_ESC_SINK_URL, TUTTE le consegne lo colpiscono: dev'essere raggiungibile o le suite
# di escalation vedrebbero consegne 'failed'. Se non c'è già un receiver, ne avviamo uno effimero.
RECV_PID=""
if [ -n "${YW_ESC_SINK_URL:-}" ] && [ "$(up "${YW_ESC_SINK_URL}")" = "000" ]; then
  export YW_ESC_SINK_LOG="${YW_ESC_SINK_LOG:-/tmp/yw_sink_shared.log}"
  : > "$YW_ESC_SINK_LOG"
  python3 - "$YW_ESC_SINK_LOG" <<'PY' &
import http.server,sys
f=open(sys.argv[1],"ab")
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get("content-length",0)); b=self.rfile.read(n)
        f.write(b+b"\n"); f.flush()
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
    def log_message(self,*a): pass
http.server.HTTPServer(("127.0.0.1",18099),H).serve_forever()
PY
  RECV_PID=$!
  for i in 1 2 3 4 5; do [ "$(up "${YW_ESC_SINK_URL}")" != "000" ] && break; sleep 1; done
  echo "sink receiver avviato (pid $RECV_PID, log $YW_ESC_SINK_LOG)"
fi
trap '[ -n "$RECV_PID" ] && kill "$RECV_PID" 2>/dev/null || true' EXIT

if [ "$MODE" = "core" ] || [ "$MODE" = "all" ]; then
  echo "== CORE =="; for s in $CORE; do run "$s"; done
fi

echo "=============================================="
echo "RUN-ALL [$MODE]: $PASS passed, $FAIL failed, $SKIP skipped"
[ "$FAIL" = "0" ] || echo "FALLITE:$FAILED_LIST"
[ "$FAIL" = "0" ]
