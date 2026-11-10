#!/usr/bin/env bash
# GoAlert parity e2e: ladder vocale, status updates, schedule (copertura), handoff,
# labels, override (assenze), maintenance, iCal, ack-sync. Gira su GLOBEX (throwaway),
# MAI su acme. Richiede core :8080 + GoAlert :8082 (Twilio abilitato per i recapiti).
set -u
BASE=${BASE:-http://localhost:8080}
GA=${GA:-http://localhost:8082}
SLUG=globex
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }

# GoAlert admin cookie (per verifiche dirette)
GACOOKIE=$(curl -s -i -X POST "$GA/api/v2/identity/providers/basic" -H "Referer: $GA/" -H "Origin: $GA" --data-urlencode "username=admin" --data-urlencode "password=goalertadmin1" | grep -i '^set-cookie:' | sed 's/^[Ss]et-[Cc]ookie: //' | cut -d';' -f1 | paste -sd'; ')
gaq(){ curl -s "$GA/api/graphql" -H "Referer: $GA/" -H 'content-type: application/json' -H "Cookie: $GACOOKIE" --data "{\"query\":\"$1\"}"; }
# assicura Twilio abilitato (necessario per creare recapiti SMS/VOICE)
gaq 'mutation{setConfig(input:[{id:\"Twilio.Enable\",value:\"true\"},{id:\"Twilio.AccountSID\",value:\"ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"},{id:\"Twilio.AuthToken\",value:\"dummytoken000000000000000000000\"},{id:\"Twilio.FromNumber\",value:\"+16505551234\"}])}' >/dev/null

# superadmin -> provisiona GoAlert per globex
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(csrf "$SJ"); sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=77616511-8bc1-4bca-9ed5-9c78dee8cf27
PROV=$(sapi -X POST "$BASE/api/superadmin/tenants/$TID/goalert/provision" -H 'content-type: application/json' -d '{}')
echo "prov: $(echo "$PROV" | head -c 160)"

# tenant_admin globex
AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
AC=$(csrf "$AJ"); aapi(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" "$@"; }

# dai un telefono E.164 all'admin (così ha un recapito SMS/VOICE)
MYID=$(aapi "$BASE/api/users" | python -c "import sys,json;print([u['id'] for u in json.load(sys.stdin) if u['email']=='admin@globex.loneworker.eu'][0])")
aapi -X POST "$BASE/api/users/$MYID" -H 'content-type: application/json' -d '{"phone":"+16505559911"}' >/dev/null

# gruppo QA
GNAME="QA parity $$"
aapi -X POST "$BASE/api/groups" -H 'content-type: application/json' -d "{\"name\":\"$GNAME\"}" >/dev/null
GID=$(aapi "$BASE/api/groups" | python -c "import sys,json;print([g['id'] for g in json.load(sys.stdin)['groups'] if g['name']=='$GNAME'][0])")
aapi -X POST "$BASE/api/groups/$GID/members" -H 'content-type: application/json' -d "{\"user_id\":\"$MYID\",\"position\":1}" >/dev/null

# config: turni + copertura orari + handoff + slack
aapi -X POST "$BASE/api/groups/$GID/oncall" -H 'content-type: application/json' \
  -d '{"mode":"turni","oncall_type":"weekly","shift":1,"coverage_mode":"orari","cov_start":"08:00","cov_end":"20:00","cov_days":"1111100","handoff_time":"09:00","handoff_day":0,"slack_notify":"off"}' >/dev/null
# impostazioni tenant: fallback vocale @3 + status updates
aapi -X POST "$BASE/api/oncall-settings" -H 'content-type: application/json' -d '{"voice_fallback_on":true,"voice_fallback_min":3,"status_updates_on":true}' >/dev/null
GOTS=$(aapi "$BASE/api/oncall-settings")
[ "$(echo "$GOTS" | jv "d['voice_fallback_min']")" = "3" ] && ok "settings salvate (voice @3)" || no "settings: $GOTS"

# SYNC
SY=$(aapi -X POST "$BASE/api/alarm/sync-goalert")
echo "sync: $(echo "$SY" | head -c 160)"
[ "$(echo "$SY" | jv "d.get('ok')")" = "True" ] && ok "sync eseguita" || { no "sync fallita: $SY"; echo "P:$PASS F:$FAIL"; exit 1; }

# verifica: utente GoAlert dell'admin ha SMS@0 + VOICE@3
GGUID=$(docker exec ywdb psql -U postgres -d youworkforce -t -c "select goalert_user_id from app.users where id='$MYID';" | tr -d ' \r\n')
CMS=$(gaq "{user(id:\\\"$GGUID\\\"){contactMethods{type} notificationRules{delayMinutes contactMethod{type}}}}")
echo "$CMS" | grep -q '"VOICE"' && ok "#2 VOICE contact method creato" || no "#2 VOICE mancante: $(echo "$CMS"|head -c 200)"
echo "$CMS" | python -c "import sys,json;r=json.load(sys.stdin)['data']['user']['notificationRules'];import sys;sys.exit(0 if any(x['delayMinutes']==3 and x['contactMethod']['type']=='VOICE' for x in r) else 1)" && ok "#1 ladder vocale @3 min" || no "#1 regola vocale @3 mancante"

# verifica: schedule creata (copertura) + regole + handoff rotation
SCID=$(docker exec ywdb psql -U postgres -d youworkforce -t -c "select goalert_schedule_id from app.alarm_groups where id='$GID';" | tr -d ' \r\n')
[ -n "$SCID" ] && ok "#3 schedule creata ($SCID)" || no "#3 schedule non creata"
if [ -n "$SCID" ]; then
  SRULES=$(gaq "{schedule(id:\\\"$SCID\\\"){targets{rules{start end weekdayFilter}}}}")
  echo "$SRULES" | grep -q '08:00' && ok "#3 regola copertura 08:00 applicata" || no "#3 regola copertura assente: $(echo "$SRULES"|head -c 200)"
fi
ROTID=$(docker exec ywdb psql -U postgres -d youworkforce -t -c "select goalert_rotation_id from app.alarm_groups where id='$GID';" | tr -d ' \r\n')
if [ -n "$ROTID" ]; then
  RST=$(gaq "{rotation(id:\\\"$ROTID\\\"){start}}")
  echo "$RST" | grep -q 'T09:00' && ok "#11 handoff rotation 09:00" || no "#11 handoff start errato: $(echo "$RST"|head -c 160)"
fi

# verifica: labels sul servizio
SVID=$(docker exec ywdb psql -U postgres -d youworkforce -t -c "select goalert_service_id from app.alarm_groups where id='$GID';" | tr -d ' \r\n')
if [ -n "$SVID" ]; then
  LBL=$(gaq "{service(id:\\\"$SVID\\\"){labels{key value}}}")
  echo "$LBL" | grep -q 'youworkforce/tenant' && ok "#12 label youworkforce/tenant" || no "#12 label mancante: $(echo "$LBL"|head -c 160)"
fi

# #7 status updates sul recapito SMS
echo "$CMS" | grep -q 'SMS' && ok "#7 recapito SMS primario presente" || no "#7 SMS mancante"

# recapiti via endpoint YW
OCC=$(aapi "$BASE/api/oncall-contacts")
echo "$OCC" | grep -q 'VOICE' && ok "endpoint recapiti mostra VOICE" || no "endpoint recapiti: $(echo "$OCC"|head -c 160)"

# #4 override (assenza): rimuovi me stesso domani
TOM=$(python -c "import datetime;print((datetime.date.today()+datetime.timedelta(days=1)).isoformat())")
DAT=$(python -c "import datetime;print((datetime.date.today()+datetime.timedelta(days=2)).isoformat())")
OVA=$(aapi -X POST "$BASE/api/oncall-overrides" -H 'content-type: application/json' -d "{\"group_id\":\"$GID\",\"start\":\"${TOM}T00:00:00Z\",\"end\":\"${DAT}T00:00:00Z\",\"remove_user_id\":\"$MYID\"}")
[ "$(echo "$OVA" | jv "d.get('ok')")" = "True" ] && ok "#4 override assenza creato" || no "#4 override: $OVA"
OVL=$(aapi "$BASE/api/oncall-overrides?group_id=$GID")
OVID=$(echo "$OVL" | python -c "import sys,json;o=json.load(sys.stdin)['overrides'];print(o[0]['id'] if o else '')" 2>/dev/null)
[ -n "$OVID" ] && ok "#4 override elencato" || no "#4 override non elencato: $(echo "$OVL"|head -c 160)"
[ -n "$OVID" ] && aapi -X POST "$BASE/api/oncall-overrides/delete" -H 'content-type: application/json' -d "{\"id\":\"$OVID\"}" >/dev/null

# #6 maintenance on/off
MON=$(aapi -X POST "$BASE/api/oncall-maintenance" -H 'content-type: application/json' -d '{"hours":1}')
[ "$(echo "$MON" | jv "d.get('active')")" = "True" ] && ok "#6 maintenance attivata" || no "#6 maintenance on: $MON"
MOFF=$(aapi -X POST "$BASE/api/oncall-maintenance" -H 'content-type: application/json' -d '{"hours":0}')
[ "$(echo "$MOFF" | jv "d.get('active')")" = "False" ] && ok "#6 maintenance terminata" || no "#6 maintenance off: $MOFF"

# #8 iCal url + fetch pubblico
CAL=$(aapi "$BASE/api/oncall-calendar/url")
CURL=$(echo "$CAL" | jv "d['url']")
ICS=$(curl -s "$BASE$CURL")
echo "$ICS" | grep -q 'BEGIN:VCALENDAR' && ok "#8 feed iCal servito" || no "#8 iCal: $(echo "$ICS"|head -c 120)"

# cleanup: elimina gruppo QA (NON tocca acme)
aapi -X DELETE "$BASE/api/groups/$GID" >/dev/null
echo "cleanup gruppo QA fatto"

echo "-------------------------------------"
echo "PARITY e2e: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
