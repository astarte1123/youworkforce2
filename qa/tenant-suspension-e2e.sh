#!/usr/bin/env bash
# SA-5 (mig 0130): sospensione del tenant programmabile e con effetto su TUTTO il servizio.
# Decisione PO: un tenant sospeso non riceve né scala allarmi; l'amministratore è avvisato prima.
#  1. programmata nel futuro -> tenant ancora attivo, avviso con data+note all'amministratore
#  2. alla data (orologio di test) il worker la applica: sessioni chiuse, login e ingress 403
#  3. riattivazione -> login e ingress di nuovo ok
#  4. sospensione immediata -> sessioni aperte chiuse subito
# Usa globex (ripristinato a 'attivo' anche se la suite si interrompe). Richiede TEST_CLOCK=1.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
code(){ curl -s -o /dev/null -w "%{http_code}" "$@"; }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }

[ "$(code -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }

S=$(mktemp); A=$(mktemp)
curl -s -o /dev/null -c "$S" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$S" | tail -1)
GID=$(curl -s -b "$S" "$BASE/api/superadmin/tenants" | python -c "import sys,json;print([t['id'] for t in json.load(sys.stdin) if t['slug']=='globex'][0])")
setst(){ curl -s -b "$S" -H 'content-type: application/json' -H "x-csrf-token: $CS" -X POST "$BASE/api/superadmin/tenants/$GID/status" -d "$1"; }
trap 'setst "{\"status\":\"attivo\"}" >/dev/null; rm -f "$S" "$A"' EXIT
setst '{"status":"attivo"}' >/dev/null
login(){ curl -s -o /dev/null -w "%{http_code}" -c "$A" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: globex' -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'; }
me(){ curl -s -b "$A" -H 'x-tenant-slug: globex' "$BASE/api/me"; }
ingest(){ local k="susp-$(date +%s%N)"; code -X POST "$BASE/api/events" -H 'content-type: application/json' \
  -H 'x-ingest-token: globex-ingest-token' -d "{\"etype\":\"sos\",\"source_event\":\"$k\",\"idem_key\":\"$k\",\"correlation\":\"$k\"}"; }

echo "== 1. sospensione programmata =="
NOW=$(curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | python -c "import sys,json;print(json.load(sys.stdin)['now'])")
AT=$(python -c "import datetime as d;n=d.datetime.fromisoformat('$NOW'.replace('Z','+00:00'));print((n+d.timedelta(hours=2)).isoformat())")
R=$(setst "{\"status\":\"sospeso\",\"at\":\"$AT\",\"note\":\"Contratto scaduto\"}")
echo "$R" | grep -q '"suspend_at"' && ok "programmata: risposta con suspend_at" || no "programma" "$R"
[ "$(q "SELECT status FROM app.tenants WHERE id='$GID'")" = "attivo" ] && ok "prima della data il tenant resta attivo" || no "stato" "non attivo"
[ "$(login)" = "200" ] && ok "prima della data il login funziona" || no "login pre" "-"
N=$(me | python -c "import sys,json;n=json.load(sys.stdin).get('suspension_notice') or {};print(n.get('note',''), bool(n.get('suspend_at')))")
[ "$N" = "Contratto scaduto True" ] && ok "avviso all'amministratore con data e motivo" || no "avviso" "$N"
C=$(ingest); [ "$C" = "200" ] && ok "prima della data gli allarmi arrivano ($C)" || no "ingest pre" "$C"

echo "== 2. alla data il worker sospende =="
code -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":7300}' >/dev/null
for i in $(seq 1 40); do [ "$(q "SELECT status FROM app.tenants WHERE id='$GID'")" = "sospeso" ] && break; sleep 2; done
[ "$(q "SELECT status FROM app.tenants WHERE id='$GID'")" = "sospeso" ] && ok "worker: sospensione applicata alla data" || no "scan" "non sospeso dopo 80s"
C=$(code -b "$A" -H 'x-tenant-slug: globex' "$BASE/api/me"); [ "$C" = "401" ] && ok "sessione aperta chiusa (401)" || no "sessione" "$C"
C=$(login); [ "$C" = "403" ] && ok "login bloccato (403)" || no "login" "$C"
C=$(ingest); [ "$C" = "403" ] && ok "allarmi in ingresso rifiutati (403)" || no "ingest" "$C"
[ "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$GID' AND action='tenant_status' AND detail->>'programmata'='true'")" -ge 1 ] \
  && ok "audit della sospensione programmata" || no "audit" "assente"

echo "== 3. riattivazione =="
setst '{"status":"attivo"}' >/dev/null
[ "$(login)" = "200" ] && ok "riattivato: login 200" || no "login riattivo" "-"
C=$(ingest); [ "$C" = "200" ] && ok "riattivato: allarmi di nuovo ricevuti" || no "ingest riattivo" "$C"
[ "$(q "SELECT COALESCE(suspend_at::text,'null') FROM app.tenants WHERE id='$GID'")" = "null" ] && ok "nessuna sospensione residua" || no "residuo" "-"

echo "== 4. sospensione immediata =="
login >/dev/null
R=$(setst '{"status":"sospeso"}')
echo "$R" | python -c "import sys,json;d=json.load(sys.stdin);sys.exit(0 if d.get('status')=='sospeso' and d.get('sessions_closed',0)>=1 else 1)" \
  && ok "sospesa subito, sessioni chiuse" || no "immediata" "$R"
C=$(code -b "$A" -H 'x-tenant-slug: globex' "$BASE/api/me"); [ "$C" = "401" ] && ok "sessione non più valida (401)" || no "sessione imm" "$C"

# teardown: chiude gli incidenti di prova
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.incidents SET state='chiuso', closed_at=now(), outcome='teardown tenant-suspension-e2e' WHERE tenant_id='$GID' AND correlation LIKE 'susp-%' AND state<>'chiuso'" >/dev/null
echo "tenant-suspension: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
