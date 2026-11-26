#!/usr/bin/env bash
# Interventi in quantità (caso reale: 60+ persone in squadre, migliaia di interventi l'anno).
#  - GET /api/orders filtra lato server: state=open|closed, from/to sull'inizio programmato, id, limit;
#    senza parametri l'elenco resta completo (compatibilità); parametri non validi -> 400
#  - i chiusi arrivano dal più recente
#  - il membro di una squadra vede gli interventi della squadra anche con i filtri
#  - calendario: l'intervento di squadra porta squadra (group_id, nome nel titolo); gli annullati
#    non compaiono; il calendario personale del membro include gli interventi della sua squadra
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="of$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Of $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"workforce\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" "$BASE$2"; }
login A "a@$SLUG.test" DirectPass9
W=$(call A POST /api/users "{\"email\":\"w@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']")
login W "w@$SLUG.test" WorkPass9
G=$(call A POST /api/user-groups '{"name":"Squadra Nord","kind":"squadra"}' | py "d['id']")
call A POST "/api/user-groups/$G/members" "{\"user_id\":\"$W\"}" >/dev/null
read TOM TOM2 YEST <<<"$(python -c "
import datetime as d;n=d.datetime.now(d.timezone.utc).replace(hour=8,minute=0,second=0,microsecond=0)
print((n+d.timedelta(days=1)).isoformat(),(n+d.timedelta(days=2)).isoformat(),(n-d.timedelta(days=1)).isoformat())")"
mk(){ call A POST /api/orders "$1" | py "d['id']"; }
O1=$(mk "{\"title\":\"Taglio erba\",\"assigned_group\":\"$G\",\"scheduled_start\":\"$TOM\"}")
O2=$(mk "{\"title\":\"Siepi\",\"assigned_group\":\"$G\",\"scheduled_start\":\"$YEST\"}")
O3=$(mk '{"title":"Da pianificare"}')
O4=$(mk "{\"title\":\"Annullato\",\"assigned_group\":\"$G\",\"scheduled_start\":\"$TOM\"}")
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.work_orders SET state='completato',work_started_at='$YEST',work_minutes=90 WHERE id='$O2'; UPDATE app.work_orders SET state='annullato' WHERE id='$O4'"

ids(){ py "','.join(sorted(o['id'] for o in d))"; }
srt(){ python -c "import sys;print(','.join(sorted(sys.argv[1:])))" "$@"; }
[ "$(call A GET /api/orders | py 'len(d)')" = "4" ] && ok "senza filtri: elenco completo (compatibile)" || no "compat" "-"
[ "$(call A GET '/api/orders?state=open' | ids)" = "$(srt $O1 $O3)" ] && ok "state=open: solo non completati né annullati" || no "open" "$(call A GET '/api/orders?state=open' | ids)"
[ "$(call A GET '/api/orders?state=closed' | ids)" = "$(srt $O2 $O4)" ] && ok "state=closed: completati e annullati" || no "closed" "-"
[ "$(call A GET '/api/orders?state=closed&limit=1' | py 'len(d)')" = "1" ] && ok "limit" || no "limit" "-"
F=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$TOM"); T=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$TOM2")
[ "$(call A GET "/api/orders?state=open&from=$F&to=$T" | ids)" = "$O1" ] && ok "from/to: solo gli appuntamenti del periodo" || no "periodo" "$(call A GET "/api/orders?state=open&from=$F&to=$T")"
[ "$(call A GET "/api/orders?id=$O3" | ids)" = "$O3" ] && ok "id: un solo intervento" || no "id" "-"
[ "$(code A '/api/orders?state=tutti')" = "400" ] && ok "state non valido: 400" || no "state 400" "-"
[ "$(code A '/api/orders?from=ieri')" = "400" ] && ok "data non valida: 400" || no "from 400" "-"
[ "$(call W GET '/api/orders?state=open' | ids)" = "$O1" ] && ok "membro della squadra: vede l'intervento di squadra con i filtri" || no "membro" "$(call W GET '/api/orders?state=open')"

WIN="from=$F&to=$T"
EV=$(call A GET "/api/calendar/events?$WIN&all=1")
echo "$EV" | py "[e for e in d if e['id']=='order-$O1'][0]['group_id']=='$G'" | grep -q True && ok "calendario: intervento con la sua squadra" || no "cal group" "$EV"
echo "$EV" | py "'Squadra Nord' in [e for e in d if e['id']=='order-$O1'][0]['title']" | grep -q True && ok "calendario: nome della squadra nel titolo" || no "cal title" "-"
echo "$EV" | py "any(e['id']=='order-$O4' for e in d)" | grep -q False && ok "calendario: gli annullati non compaiono" || no "cal annullato" "-"
call W GET "/api/calendar/events?$WIN" | py "any(e['id']=='order-$O1' for e in d)" | grep -q True \
  && ok "calendario del membro: include gli interventi della squadra" || no "cal membro" "$(call W GET "/api/calendar/events?$WIN")"

echo "orders-filters: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
