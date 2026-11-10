#!/usr/bin/env bash
# IOT/THINGSBOARD — motore regole sensori parametrico (da YouWorkforce).
#  regola metrica+operatore+soglia -> etype; sfora -> incidente (catena allarmi); ok -> telemetria.
#  vale per ThingsBoard (/ingress/thingsboard) e IoT generico (/events, eleva l'etype).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
cnt(){ sql "SELECT count(*) FROM app.incidents WHERE tenant_id=$TIDQ AND etype='sensore_critico'"; }

AD=$(mktemp); curl -s -o /dev/null -c "$AD" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
AC=$(awk '$6=="yw_csrf"{print $7}' "$AD" | tail -1)
api(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $AC" "$@"; }
TOK=$(sql "SELECT token FROM app.ingest_tokens WHERE tenant_id=$TIDQ AND source='iot' LIMIT 1")

# crea una regola di test univoca (metrica e2e_co)
RID=$(api -H 'content-type: application/json' -X POST "$BASE/api/sensor-rules" -d '{"metric":"e2e_co","op":">=","threshold":35,"etype":"sensore_critico","label":"E2E CO"}' | python -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$RID" ] && ok "regola creata (e2e_co >= 35 -> sensore_critico)" || no "regola non creata" "-"
# operatore senza soglia -> 400
api -H 'content-type: application/json' -X POST "$BASE/api/sensor-rules" -d '{"metric":"x","op":">=","etype":"sensore_critico"}' | grep -qi "soglia" && ok "operatore numerico senza soglia respinto" || no "validazione soglia assente" "-"
# etype non ammesso -> 400
api -H 'content-type: application/json' -X POST "$BASE/api/sensor-rules" -d '{"metric":"x","op":"is_true","etype":"pippo"}' | grep -qi "etype" && ok "etype non ammesso respinto" || no "validazione etype assente" "-"

# ThingsBoard: sfora -> incidente
B=$(cnt)
curl -s -X POST "$BASE/api/ingress/thingsboard?token=$TOK&device=e2e" -H 'content-type: application/json' -d '{"e2e_co":40}' >/dev/null
sleep 1; A=$(cnt)
[ "$A" -eq "$((B+1))" ] && ok "TB: e2e_co=40 -> incidente sensore_critico" || no "TB sfora non ha aperto incidente" "$B->$A"
# ThingsBoard: sotto soglia -> telemetria, niente incidente
B=$(cnt)
curl -s -X POST "$BASE/api/ingress/thingsboard?token=$TOK&device=e2e" -H 'content-type: application/json' -d '{"e2e_co":5}' >/dev/null
sleep 1; A=$(cnt)
[ "$A" -eq "$B" ] && ok "TB: e2e_co=5 -> telemetria (nessun incidente)" || no "sotto soglia ha aperto incidente" "$B->$A"
LAST=$(sql "SELECT etype FROM app.events WHERE tenant_id=$TIDQ AND source='thingsboard' ORDER BY received_ts DESC LIMIT 1")
[ "$LAST" = "telemetria" ] && ok "evento sotto soglia = telemetria" || no "etype sotto soglia errato" "$LAST"

# IoT generico: etype telemetria ma payload sfora -> elevato
IDEM="e2e-iot-$RANDOM"
curl -s -X POST "$BASE/api/events" -H "x-ingest-token: $TOK" -H 'content-type: application/json' -d "{\"source_event\":\"$IDEM\",\"etype\":\"telemetria\",\"payload\":{\"e2e_co\":99}}" >/dev/null
sleep 1
[ "$(sql "SELECT etype FROM app.events WHERE source_event='$IDEM'")" = "sensore_critico" ] && ok "IoT generico: payload sfora -> etype elevato a sensore_critico" || no "IoT generico non elevato" "-"

# cleanup regola di test
api -X DELETE "$BASE/api/sensor-rules/$RID" >/dev/null
[ "$(sql "SELECT count(*) FROM app.sensor_rules WHERE id='$RID'")" = "0" ] && ok "regola eliminata" || no "delete regola fallito" "-"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
