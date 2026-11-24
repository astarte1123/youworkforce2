#!/usr/bin/env bash
# TA-5 + TA-9: anonimizzazione GDPR completa ed export dal catalogo unico.
# Dopo l'anonimizzazione la persona non è più identificabile né destinataria di allarmi:
# email/telefono, recapiti, gruppi di allarme, turni di reperibilità futuri, badge assegnato,
# posizioni dell'app personale, email nell'audit e nei log di consegna. Lo storico (incidenti,
# turni passati) resta legato all'identificativo pseudonimo.
# Tenant usa-e-getta, eliminato a fine suite.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="gdpr$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"GDPR $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/a" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" \
  -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
AC=$(awk '$6=="yw_csrf"{print $7}' "$J/a" | tail -1)
A(){ curl -s -b "$J/a" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' "$@"; }

EM="mario.rossi.$R@$SLUG.test"; PH="+39333$R"
UID1=$(A -X POST "$BASE/api/users" -d "{\"email\":\"$EM\",\"role\":\"reperibile\",\"password\":\"Pass$R\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
A -X POST "$BASE/api/users/$UID1" -d "{\"phone\":\"$PH\"}" >/dev/null
docker exec -i ywdb psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -q >/dev/null <<SQL
INSERT INTO app.user_contact_methods(tenant_id,user_id,kind,value) VALUES('$TID','$UID1','email','$EM');
WITH g AS (INSERT INTO app.alarm_groups(tenant_id,name) VALUES('$TID','Squadra notte') RETURNING id)
INSERT INTO app.alarm_group_members(tenant_id,group_id,user_id) SELECT '$TID',id,'$UID1' FROM g;
INSERT INTO app.oncall(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$UID1',now()-interval '10 days',now()-interval '9 days');
INSERT INTO app.oncall(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$UID1',now()+interval '1 day',now()+interval '2 days');
INSERT INTO app.devices(tenant_id,source,external_id,label,assigned_user,enabled) VALUES('$TID','gps','gdpr-$R','Badge prova','$UID1',true);
INSERT INTO app.devices(tenant_id,source,external_id,label,assigned_user,enabled) VALUES('$TID','pwa','pwa-$UID1','App $EM','$UID1',true);
INSERT INTO app.device_positions(tenant_id,device_id,lat,lon,fixed_at)
  SELECT '$TID',id,45.1,9.1,now() FROM app.devices WHERE tenant_id='$TID' AND source='pwa';
WITH i AS (INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,closed_at)
            VALUES('$TID','sos','chiuso',1,'gdpr-$R',now(),now()) RETURNING id)
INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,state) SELECT '$TID',id,1,'sms','$PH','sent' FROM i;
SQL
[ "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND detail::text LIKE '%$EM%'")" -ge 1 ] \
  && ok "setup: l'audit contiene l'email in chiaro" || no "setup audit" "-"

echo "== TA-9 export =="
X=$(A "$BASE/api/gdpr/export/$UID1")
echo "$X" | python -c "import sys,json;d=json.load(sys.stdin)['altri_dati'];sys.exit(0 if d['recapiti'] and d['gruppi_allarme'] and d['dispositivi_assegnati'] is not None else 1)" \
  && ok "export include recapiti, gruppi di allarme e altre categorie" || no "export" "$(echo "$X" | head -c 200)"

echo "== TA-5 anonimizzazione =="
C=$(A -o /dev/null -w "%{http_code}" -X POST "$BASE/api/gdpr/anonymize/$UID1"); [ "$C" = "200" ] && ok "anonimizzazione eseguita" || no "anonymize" "$C"
[ "$(q "SELECT (email LIKE 'anon-%' AND phone IS NULL AND anonymized_at IS NOT NULL) FROM app.users WHERE id='$UID1'")" = "t" ] && ok "email e telefono rimossi" || no "users" "-"
[ "$(q "SELECT count(*) FROM app.user_contact_methods WHERE user_id='$UID1'")" = "0" ] && ok "recapiti cancellati" || no "recapiti" "-"
[ "$(q "SELECT count(*) FROM app.alarm_group_members WHERE user_id='$UID1'")" = "0" ] && ok "tolto dai gruppi di allarme (non più destinatario)" || no "gruppi" "-"
[ "$(q "SELECT count(*) FROM app.oncall WHERE user_id='$UID1' AND ends_at>now()")" = "0" ] && ok "turni di reperibilità futuri rimossi" || no "oncall futuri" "-"
[ "$(q "SELECT count(*) FROM app.oncall WHERE user_id='$UID1' AND ends_at<now()")" = "1" ] && ok "storico reperibilità passato conservato (pseudonimo)" || no "oncall passati" "-"
[ "$(q "SELECT count(*) FROM app.devices WHERE assigned_user='$UID1'")" = "0" ] && ok "badge e app liberati" || no "devices" "-"
[ "$(q "SELECT count(*) FROM app.device_positions p JOIN app.devices d ON d.id=p.device_id WHERE d.tenant_id='$TID' AND d.source='pwa'")" = "0" ] && ok "posizioni dell'app personale cancellate" || no "posizioni" "-"
[ "$(q "SELECT count(*) FROM app.devices WHERE tenant_id='$TID' AND label LIKE '%$EM%'")" = "0" ] && ok "etichetta del dispositivo senza email" || no "label" "-"
[ "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND (actor='$EM' OR detail::text LIKE '%$EM%')")" = "0" ] && ok "email rimossa dall'audit" || no "audit" "-"
[ "$(q "SELECT count(*) FROM app.esc_delivery_log WHERE tenant_id='$TID' AND to_value='$PH'")" = "0" ] && ok "telefono rimosso dai log di consegna" || no "delivery log" "-"
C=$(A -o /dev/null -w "%{http_code}" -X POST "$BASE/api/gdpr/anonymize/$UID1"); [ "$C" = "409" ] && ok "seconda anonimizzazione: già anonimizzato (409)" || no "idempotenza" "$C"

echo "gdpr-anonymize-full: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
