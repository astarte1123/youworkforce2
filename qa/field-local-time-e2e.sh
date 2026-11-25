#!/usr/bin/env bash
# Correzioni dalla revisione dei manuali di ronde e consuntivi (26/11/2026):
# 1) registro ore: giorni nell'ora locale dell'azienda (non UTC);
# 2) messaggio «ronda già in corso» con l'ora della sede (non UTC);
# 3) listino disponibile anche col solo modulo Consuntivi;
# 4) modifica di un percorso di ronda: nome obbligatorio.
# Tenant usa-e-getta (fuso Europe/Rome).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -Atc "$1"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))')
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
mkt(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$1\",\"name\":\"$1\",\"admin_email\":\"a@$1.test\",\"admin_password\":\"DirectPass9\",\"modules\":$2}" | py "d.get('id','')"; }
S1="fl$R"; S2="fm$R"
T1=$(mkt "$S1" '["cantieri","giri"]'); T2=$(mkt "$S2" '["consuntivi"]')
trap 'for x in "$T1:$S1" "$T2:$S2"; do curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/${x%%:*}" -d "{\"slug\":\"${x##*:}\"}"; done; rm -rf "$J"' EXIT
[ -n "$T1" ] && [ -n "$T2" ] || { echo "FAIL crea tenant"; exit 1; }
sql "UPDATE app.tenants SET timezone='Europe/Rome' WHERE id IN ('$T1','$T2')" >/dev/null
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $1" -d "{\"email\":\"a@$1.test\",\"password\":\"DirectPass9\"}"; }
as(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $1" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
asc(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $1" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login "$S1"; login "$S2"

# 1) presenza 00:30-02:30 ora italiana del 10/11 (= 23:30-01:30 UTC del 9/11): conta nel 10/11
CID=$(as "$S1" POST /api/commesse "{\"code\":\"C-$R\",\"name\":\"Cantiere\"}" | py "d['id']")
PUID=$(sql "SELECT id FROM app.users WHERE tenant_id='$T1' LIMIT 1")
sql "INSERT INTO app.commessa_presenze(tenant_id,commessa_id,user_id,clock_in,clock_out,source) VALUES('$T1','$CID','$PUID','2026-11-09 23:30:00+00','2026-11-10 01:30:00+00','manuale')" >/dev/null
WH=$(as "$S1" GET "/api/work-hours?from=2026-11-10&to=2026-11-10")
echo "$WH" | py "[(str(x['day']),x['effective_min']) for x in d['days']]" | grep -q "('2026-11-10', 120)" \
  && ok "registro ore: la presenza dopo mezzanotte (ora italiana) conta nel giorno giusto" || no "giorno locale" "$(echo "$WH" | py "d['days']")"
[ "$(as "$S1" GET "/api/work-hours?from=2026-11-09&to=2026-11-09" | py "len(d['entries'])")" = "0" ] && ok "registro ore: niente nel giorno prima" || no "giorno prima" "-"

# 2) «ronda già in corso»: ora della sede
RID=$(as "$S1" POST /api/patrol-routes '{"name":"Perimetro"}' | py "d['id']")
as "$S1" POST "/api/patrol-routes/$RID/rounds" '{}' >/dev/null
MSG=$(as "$S1" POST "/api/patrol-routes/$RID/rounds" '{}')
EXP=$(sql "SELECT to_char(started_at AT TIME ZONE 'Europe/Rome','HH24:MI') FROM app.patrol_rounds WHERE route_id='$RID'")
echo "$MSG" | grep -q "alle $EXP" && ! echo "$MSG" | grep -q "UTC" && ok "ronda già in corso: ora locale della sede ($EXP)" || no "ora messaggio" "$MSG / $EXP"

# 3) listino col solo modulo Consuntivi
[ "$(asc "$S2" GET /api/price-items)" = "200" ] && ok "listino disponibile col solo modulo Consuntivi" || no "listino consuntivi" "$(asc "$S2" GET /api/price-items)"
[ "$(asc "$S2" POST /api/price-items '{"code":"MO","name":"Manodopera","unit":"h","unit_price":35}')" = "200" ] && ok "voce di listino creata col solo modulo Consuntivi" || no "crea voce" "-"

# 4) percorso: nome vuoto rifiutato anche in modifica
[ "$(asc "$S1" POST "/api/patrol-routes/$RID" '{"name":"  "}')" = "400" ] && ok "percorso di ronda: nome vuoto rifiutato in modifica" || no "nome vuoto" "-"
[ "$(sql "SELECT name FROM app.patrol_routes WHERE id='$RID'")" = "Perimetro" ] && ok "il nome resta quello di prima" || no "nome cambiato" "-"
echo "field-local-time: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
