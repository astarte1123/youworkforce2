#!/usr/bin/env bash
# CA-6 (decisione PO 25/11/2026): listino unico in centesimi, tariffe di manodopera per QUALIFICA
# con validità nel tempo; consuntivo e costi di commessa leggono da lì. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="lq$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Lq $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"consuntivi\",\"qualifiche\",\"cantieri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
UE=$(mk ele); UG=$(mk gen)
login E "ele@$SLUG.test" WorkPass9; login G "gen@$SLUG.test" WorkPass9
QE=$(call A POST /api/qualifications '{"name":"Elettricista"}' | py "d['id']")
call A POST "/api/users/$UE/qualifications" "{\"qualification_id\":\"$QE\"}" >/dev/null
TODAY=$(python -c "import datetime;print(datetime.date.today())"); YEST=$(python -c "import datetime;print(datetime.date.today()-datetime.timedelta(days=1))")
call A POST /api/price-items '{"name":"Manodopera","category":"manodopera","unit":"ora","unit_price":30}' >/dev/null
call A POST /api/price-items "{\"name\":\"Manodopera elettricista (vecchia)\",\"category\":\"manodopera\",\"unit\":\"ora\",\"unit_price\":40,\"qualification_id\":\"$QE\",\"valid_to\":\"$YEST\"}" >/dev/null
P3=$(call A POST /api/price-items "{\"name\":\"Manodopera elettricista\",\"category\":\"manodopera\",\"unit\":\"ora\",\"unit_price\":45.10,\"qualification_id\":\"$QE\",\"valid_from\":\"$TODAY\"}" | py "d['id']")

L=$(call A GET /api/price-items)
echo "$L" | py "[x for x in d if x['id']=='$P3'][0]['price_cents']==4510 and [x for x in d if x['id']=='$P3'][0]['qualification']=='Elettricista'" | grep -q True \
  && ok "prezzo salvato in centesimi (45,10 € = 4510) con la qualifica" || no "cents" "$L"
[ "$(q "SELECT unit_price::numeric(10,2) FROM app.price_list_items WHERE id='$P3'")" = "45.10" ] \
  && ok "prezzo in euro calcolato dai centesimi (nessun arrotondamento in virgola mobile)" || no "unit_price" "$(q "SELECT unit_price FROM app.price_list_items WHERE id='$P3'")"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $(awk '$6=="yw_csrf"{print $7}' "$J/A"|tail -1)" -H 'content-type: application/json' -X POST "$BASE/api/price-items" -d "{\"name\":\"X\",\"unit_price\":-1}")
[ "$C" = "400" ] && ok "prezzo negativo rifiutato" || no "neg" "$C"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $(awk '$6=="yw_csrf"{print $7}' "$J/A"|tail -1)" -H 'content-type: application/json' -X POST "$BASE/api/price-items" -d "{\"name\":\"X\",\"valid_from\":\"$TODAY\",\"valid_to\":\"$YEST\"}")
[ "$C" = "400" ] || [ "$C" = "409" ] && ok "validità con fine prima dell'inizio rifiutata ($C)" || no "validity" "$C"

rate(){ local ts; ts=$(call "$1" POST /api/timesheets '{}' | py "d['id']")
  call "$1" POST "/api/timesheets/$ts/lines" "{\"activity\":\"lavoro\",\"work_date\":\"$TODAY\",\"minutes\":60,\"rate_cents\":99999}" >/dev/null
  q "SELECT rate_cents FROM app.timesheet_lines WHERE timesheet_id='$ts'"; }
[ "$(rate E)" = "4510" ] && ok "elettricista: tariffa della sua qualifica valida oggi (45,10 €), non quella scaduta né quella indicata" || no "rate qual" "$(rate E)"
[ "$(rate G)" = "3000" ] && ok "senza qualifica: tariffa di manodopera per tutti (30 €)" || no "rate gen" "$(rate G)"
[ "$(q "SELECT app.yw_labor_rate_cents('$TID','$UE','$YEST')")" = "4000" ] && ok "ieri valeva la tariffa precedente (40 €): validità nel tempo" || no "storico" "$(q "SELECT app.yw_labor_rate_cents('$TID','$UE','$YEST')")"

# costi di commessa: 2 ore di presenza dell'elettricista (oggi 8-10 ora italiana: la tariffa vale per il
# giorno LOCALE della presenza, quindi niente now()-2h che dopo mezzanotte cade nel giorno prima)
CM=$(call A POST /api/commesse "{\"code\":\"C-$R\",\"name\":\"Cantiere $R\"}" | py "d['id']")
docker exec -i ywdb psql -U postgres -d youworkforce -qc "INSERT INTO app.commessa_presenze(tenant_id,commessa_id,user_id,clock_in,clock_out) VALUES('$TID','$CM','$UE',('$TODAY 08:00'::timestamp AT TIME ZONE 'Europe/Rome'),('$TODAY 10:00'::timestamp AT TIME ZONE 'Europe/Rome'))"
call A GET "/api/commesse/$CM" | py "d['cost']['labor_cost']" | grep -q "^90.2$" && ok "costo di commessa: 2 h × 45,10 € = 90,20 € dalla qualifica" || no "commessa" "$(call A GET "/api/commesse/$CM" | py "d['cost']")"
echo "listino-qualifica: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
