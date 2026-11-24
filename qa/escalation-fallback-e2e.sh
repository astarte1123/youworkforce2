#!/usr/bin/env bash
# SL-1 + SL-12 (mig 0131): rete di ultima istanza dell'escalation.
#  - allarme critico senza alcun gruppo -> rete (motivo nessun_gruppo)
#  - modo seriale: il secondo contatto parte solo dopo l'intervallo; la presa in carico ferma la catena
#  - modo parallelo: tutti i contatti subito
#  - fuori fascia di copertura: cronologia "nessun destinatario" + rete (motivo fuori_copertura)
#  - validazione dei contatti
# Tenant usa-e-getta (eliminato a fine suite). Richiede TEST_CLOCK=1. Le consegne vanno al sink o in
# modalità simulata: nessun recapito reale (domini .test, numeri +3900000…).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
sqlx(){ docker exec -i ywdb psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -tAc "$1"; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
ADV(){ curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="fb$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"FB $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/a" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" \
  -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
AC=$(awk '$6=="yw_csrf"{print $7}' "$J/a" | tail -1)
A(){ curl -s -b "$J/a" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' "$@"; }
Ac(){ A -o /dev/null -w "%{http_code}" "$@"; }
RUN(){ A -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null; }
newinc(){ sqlx "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) VALUES('$TID','sos','aperto',3,'fb-$R-$1',now()) RETURNING id" | head -1 | tr -d '[:space:]'; }
fbn(){ q "SELECT count(*) FROM app.esc_delivery_log WHERE tenant_id='$TID' AND incident_id='$1' AND step=-1"; }

echo "== configurazione =="
C=$(Ac -X PUT "$BASE/api/escalation/fallback" -d '{"mode":"seriale","interval_s":60,"contacts":[{"channel":"sms","value":"non-un-numero"}]}')
[ "$C" = "400" ] && ok "telefono non valido rifiutato (400)" || no "validazione" "$C"
C=$(Ac -X PUT "$BASE/api/escalation/fallback" -d '{"mode":"seriale","interval_s":60,"contacts":[{"channel":"email","value":"capoturno@'$SLUG'.test","label":"Capoturno"},{"channel":"sms","value":"+390000000'"${R:0:3}"'"}]}')
[ "$C" = "200" ] && ok "rete salvata: email + SMS, uno dopo l'altro ogni 60 s" || no "salva" "$C"
N=$(A "$BASE/api/escalation/fallback" | python -c "import sys,json;d=json.load(sys.stdin);print(d['mode'],len(d['contacts']))")
[ "$N" = "seriale 2" ] && ok "configurazione riletta" || no "rilettura" "$N"

echo "== SOS senza gruppo: seriale + presa in carico =="
I1=$(newinc a); RUN
[ "$(q "SELECT reason FROM app.esc_fallback_state WHERE incident_id='$I1'")" = "nessun_gruppo" ] && ok "rete avviata: motivo nessun gruppo" || no "start" "-"
[ "$(fbn $I1)" = "1" ] && ok "seriale: avvisato subito solo il primo contatto" || no "primo" "$(fbn $I1)"
ADV 61; RUN
[ "$(fbn $I1)" = "2" ] && ok "dopo 60 s avvisato il secondo contatto" || no "secondo" "$(fbn $I1)"
I2=$(newinc b); RUN
[ "$(fbn $I2)" = "1" ] && ok "secondo SOS: primo contatto avvisato" || no "I2 primo" "$(fbn $I2)"
C=$(Ac -X POST "$BASE/api/incidents/$I2/ack" -d '{}'); [ "$C" = "200" ] || no "ack" "$C"
ADV 61; RUN
[ "$(fbn $I2)" = "1" ] && ok "presa in carico: la catena si ferma (nessun secondo avviso)" || no "stop ack" "$(fbn $I2)"
[ "$(q "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$I1' AND kind='esc_rete_ultima_istanza'")" = "1" ] && ok "cronologia: rete di ultima istanza registrata" || no "timeline" "-"

echo "== modo parallelo =="
A -X PUT "$BASE/api/escalation/fallback" -d '{"mode":"parallelo","contacts":[{"channel":"email","value":"capoturno@'$SLUG'.test"},{"channel":"email","value":"direzione@'$SLUG'.test"}]}' >/dev/null
I3=$(newinc c); RUN
[ "$(fbn $I3)" = "2" ] && ok "parallelo: tutti i contatti avvisati subito" || no "parallelo" "$(fbn $I3)"

echo "== fuori fascia di copertura =="
UIDR=$(A -X POST "$BASE/api/users" -d "{\"email\":\"rep@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"RepPass99\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
A -X POST "$BASE/api/users/$UIDR" -d '{"phone":"+390000000999"}' >/dev/null
GID=$(A -X POST "$BASE/api/groups" -d '{"name":"Notte"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
A -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$UIDR\",\"position\":1}" >/dev/null
# fascia di copertura di un minuto, lontana dall'ora corrente (di test): ora siamo FUORI fascia
H=$(curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | python -c "import sys,json,datetime as d;n=d.datetime.fromisoformat(json.load(sys.stdin)['now'].replace('Z','+00:00'));print(((n.hour+12)%24))")
sqlx "UPDATE app.alarm_groups SET coverage_mode='orari', cov_start='$(printf %02d $H):00', cov_end='$(printf %02d $H):01', cov_days='1111111', oncall_tz='UTC' WHERE id='$GID'" >/dev/null
I4=$(sqlx "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) VALUES('$TID','sos','aperto',3,'fb-$R-d',now(),'$GID') RETURNING id" | head -1 | tr -d '[:space:]')
RUN
[ "$(q "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$I4' AND kind='esc_nessun_destinatario'")" -ge 1 ] && ok "fuori fascia: cronologia 'nessun destinatario'" || no "nessun dest" "-"
[ "$(q "SELECT reason FROM app.esc_fallback_state WHERE incident_id='$I4'")" = "fuori_copertura" ] && ok "fuori fascia: rete avviata (motivo fuori copertura)" || no "fuori cop" "$(q "SELECT reason FROM app.esc_fallback_state WHERE incident_id='$I4'")"
[ "$(fbn $I4)" = "2" ] && ok "fuori fascia: contatti della rete avvisati" || no "fuori cop consegne" "$(fbn $I4)"

echo "escalation-fallback: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
