#!/usr/bin/env bash
# SL-8 + SL-9: risposta SMS del reperibile e testo della notifica.
#  - il messaggio dice il tipo d'allarme in chiaro, il lavoratore e come rispondere con il codice
#  - «NON OK» non vale come presa in carico (la parola chiave deve essere la prima)
#  - con più allarmi aperti, «OK» senza codice non prende in carico nulla e chiede il codice
#  - «OK <codice>» prende in carico esattamente quell'allarme; poi «OK» vale per l'unico rimasto
#  - un numero non avvisato non può prendere in carico nulla
# Tenant usa-e-getta (eliminato a fine suite). Consegne al sink/simulate, numeri fittizi.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
sqlx(){ docker exec -i ywdb psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -tAc "$1"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sms$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"SMS $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/a" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" \
  -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
AC=$(awk '$6=="yw_csrf"{print $7}' "$J/a" | tail -1)
A(){ curl -s -b "$J/a" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' "$@"; }
PH="+3900001${R}"
RID=$(A -X POST "$BASE/api/users" -d "{\"email\":\"rep@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"RepPass99\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
A -X POST "$BASE/api/users/$RID" -d "{\"phone\":\"$PH\"}" >/dev/null
GID=$(A -X POST "$BASE/api/groups" -d '{"name":"Turno"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
A -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$RID\",\"position\":1}" >/dev/null
newinc(){ sqlx "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id,worker_user_id) VALUES('$TID','$1','aperto',3,'sms-$R-$1',now(),'$GID','$RID') RETURNING id" | head -1 | tr -d '[:space:]'; }
I1=$(newinc sos); I2=$(newinc man_down)
A -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
[ "$(q "SELECT count(DISTINCT incident_id) FROM app.esc_delivery_log WHERE tenant_id='$TID' AND to_value='$PH'")" = "2" ] && ok "il reperibile è avvisato per due allarmi" || no "consegne" "-"
sms(){ curl -s -X POST "$BASE/api/ingress/twilio/sms" -H 'content-type: application/x-www-form-urlencoded' --data-urlencode "From=$1" --data-urlencode "Body=$2"; }
st(){ q "SELECT state FROM app.incidents WHERE id='$1'"; }
C1=${I1:0:8}

if [ -n "${YW_ESC_SINK_LOG:-}" ] && [ -f "$YW_ESC_SINK_LOG" ]; then
  grep -a "$C1" "$YW_ESC_SINK_LOG" | grep -q "SOS / richiesta soccorso" && grep -a "$C1" "$YW_ESC_SINK_LOG" | grep -q "Rispondi OK $C1" \
    && ok "messaggio leggibile: tipo in chiaro e istruzioni con il codice" || no "testo" "$(grep -a "$C1" "$YW_ESC_SINK_LOG" | tail -1 | cut -c1-200)"
fi
sms "$PH" "NON OK" >/dev/null
[ "$(st $I1)$(st $I2)" = "apertoaperto" ] && ok "«NON OK» non prende in carico" || no "non ok" "$(st $I1)/$(st $I2)"
M=$(sms "$PH" "OK")
[ "$(st $I1)$(st $I2)" = "apertoaperto" ] && echo "$M" | grep -q "codice" && ok "«OK» con due allarmi aperti: chiede il codice, nessuna presa in carico" || no "ambiguo" "$M"
sms "+3999999${R}" "OK $C1" >/dev/null
[ "$(st $I1)" = "aperto" ] && ok "un numero mai avvisato non prende in carico" || no "estraneo" "$(st $I1)"
M=$(sms "$PH" "OK $C1")
[ "$(st $I1)" = "preso_in_carico" ] && [ "$(st $I2)" = "aperto" ] && ok "«OK <codice>» prende in carico solo quell'allarme" || no "con codice" "$(st $I1)/$(st $I2)"
sms "$PH" "ok" >/dev/null
[ "$(st $I2)" = "preso_in_carico" ] && ok "«OK» vale per l'unico allarme ancora aperto" || no "unico" "$(st $I2)"
sms "$PH" "chiudi ${I2:0:8}" >/dev/null
[ "$(st $I2)" = "chiuso" ] && ok "«CHIUDI <codice>» chiude quell'allarme" || no "chiudi" "$(st $I2)"

echo "sms-ack: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
