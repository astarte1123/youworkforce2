#!/usr/bin/env bash
# CA-4 (decisione PO 25/11/2026): registro unico delle ore (vista app.work_hours, mig 0158).
#  - fonti: intervento (fatturabile), ronda, presenza di cantiere, turno (non fatturabili)
#  - sovrapposizione: vince la fonte più specifica (intervento > ronda > presenza > turno)
#  - turno pianificato confrontato con quello svolto; il lavoratore vede solo le sue ore
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
qq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qtAc "$1" >/dev/null; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="wh$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Wh $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"cantieri\",\"workforce\",\"consuntivi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
U1=$(mk w1); U2=$(mk w2); login W1 "w1@$SLUG.test" WorkPass9
DAY=$(python -c "import datetime;print(datetime.date.today()-datetime.timedelta(days=1))")
T(){ echo "${DAY}T$1:00+00:00"; }
CM=$(call A POST /api/commesse "{\"code\":\"C-$R\",\"name\":\"Cantiere $R\"}" | py "d['id']")
# w1: turno 07-16, presenza di cantiere 08-12, dentro un intervento 09-10 (60 min)
qq "INSERT INTO app.shifts(tenant_id,user_id,state,started_at,ended_at) VALUES('$TID','$U1','terminato','$(T 07:00)','$(T 16:00)')"
qq "INSERT INTO app.commessa_presenze(tenant_id,commessa_id,user_id,clock_in,clock_out) VALUES('$TID','$CM','$U1','$(T 08:00)','$(T 12:00)')"
qq "INSERT INTO app.work_orders(tenant_id,title,assigned_user,state,work_started_at,work_minutes) VALUES('$TID','Guasto quadro','$U1','completato','$(T 09:00)',60)"
qq "INSERT INTO app.shift_plans(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$U1','$(T 06:30)','$(T 16:00)')"
qq "INSERT INTO app.shift_plans(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$U2','$(T 08:00)','$(T 12:00)')"

D=$(call A GET "/api/work-hours?from=$DAY&to=$DAY")
day1(){ echo "$D" | py "[x['$1'] for x in d['days'] if x['email']=='w1@$SLUG.test'][0]"; }
[ "$(day1 effective_min)" = "540" ] && ok "giornata di w1: 9 h contate una sola volta (turno 07-16 con presenza e intervento dentro)" || no "effettive" "$(day1 effective_min)"
[ "$(day1 billable_min)" = "60" ] && ok "fatturabile solo l'intervento (60 min)" || no "fatturabili" "$(day1 billable_min)"
[ "$(day1 overlap_min)" = "300" ] && ok "sovrapposizioni visibili (presenza dentro il turno 4 h + intervento dentro la presenza 1 h)" || no "overlap" "$(day1 overlap_min)"
echo "$D" | py "[x['effective_min'] for x in d['entries'] if x['kind']=='presenza'][0]" | grep -q "^180$" && ok "la presenza conta 3 h: l'ora dell'intervento vince" || no "precedenza" "-"
echo "$D" | py "[x for x in d['plans'] if x['email']=='w1@$SLUG.test'][0]['late_min']" | grep -q "^30$" && ok "turno pianificato alle 6:30, lavoro iniziato alle 7:00: 30 min di ritardo" || no "ritardo" "$(echo "$D" | py "d['plans']")"
echo "$D" | py "[x for x in d['plans'] if x['email']=='w2@$SLUG.test'][0]['missing']" | grep -q True && ok "turno pianificato senza nessun lavoro registrato: segnalato" || no "mancante" "-"
W=$(call W1 GET "/api/work-hours?from=$DAY&to=$DAY")
echo "$W" | py "d['all'] is False and all(x['email']=='w1@$SLUG.test' for x in d['entries'])" | grep -q True && ok "il lavoratore vede solo le sue ore" || no "visibilità" "$W"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" "$BASE/api/work-hours?from=2026-01-01&to=2026-12-31"); [ "$C" = "400" ] && ok "periodo oltre 62 giorni rifiutato" || no "periodo" "$C"
echo "work-hours: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
