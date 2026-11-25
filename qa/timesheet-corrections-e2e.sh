#!/usr/bin/env bash
# Consuntivi: correzione (CA-8), valori e tariffe (CA-14), ponte dall'intervento (CA-18).
#  CA-18 il consuntivo si genera solo a intervento completato; i minuti si riallineano se cambiano
#  CA-14 chi consuntiva non sceglie la tariffa (vale il listino); niente minuti/spese/valute non
#        valide; i totali non sommano valute diverse; chi approva fissa la tariffa mancante
#  CA-8  un consuntivo «da correggere» si corregge (modifica/cancella righe e spese); la nota del
#        responsabile non cancella quella del consuntivo; lo storico conserva le note
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
qq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="tc$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Tc $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"consuntivi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"$2\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
UT=$(mk tec lavoratore); UR=$(mk resp responsabile); UP=$(mk plan pianificatore)
login T "tec@$SLUG.test" WorkPass9; login RS "resp@$SLUG.test" WorkPass9; login P "plan@$SLUG.test" WorkPass9
IT=$(call A POST /api/price-items '{"name":"Ora tecnico","category":"manodopera","unit":"ora","unit_price":30}' | py "d['id']")
O=$(call P POST /api/orders "{\"title\":\"Caldaia $R\",\"assigned_user\":\"$UT\"}" | py "d['id']")
ver(){ q "SELECT version FROM app.work_orders WHERE id='$O'"; }
call T POST "/api/orders/$O/report" '{"work_minutes":60,"materials":[{"descr":"guarnizione","qta":2,"unita":"pz"}]}' >/dev/null

echo "== CA-18 ponte dall'intervento =="
C=$(code T POST "/api/orders/$O/timesheet" '{}'); [ "$C" = "409" ] && ok "intervento non completato: niente consuntivo (409)" || no "non completato" "$C"
call T POST "/api/orders/$O/start" "{\"version\":$(ver)}" >/dev/null
call T POST "/api/orders/$O/complete" "{\"version\":$(ver),\"outcome\":\"ok\"}" >/dev/null
TS=$(call T POST "/api/orders/$O/timesheet" '{}' | py "d['timesheet_id']")
[ -n "$TS" ] && ok "intervento completato: consuntivo generato" || no "ponte" "-"
[ "$(q "SELECT rate_cents FROM app.timesheet_lines WHERE timesheet_id='$TS'")" = "3000" ] && ok "la riga dall'intervento prende la tariffa del listino (30 €/h)" || no "tariffa ponte" "$(q "SELECT rate_cents FROM app.timesheet_lines WHERE timesheet_id='$TS'")"
qq "UPDATE app.work_orders SET work_minutes=75 WHERE id='$O'"
call T POST "/api/orders/$O/timesheet" '{}' | py "d['line_updated']" | grep -q True && ok "minuti cambiati: la riga si riallinea" || no "riallinea" "-"
[ "$(q "SELECT minutes FROM app.timesheet_lines WHERE timesheet_id='$TS'")" = "75" ] && ok "riga a 75 minuti" || no "minuti" "-"

echo "== CA-14 valori e tariffe =="
C=$(code T POST "/api/timesheets/$TS/lines" '{"activity":"x","work_date":"2026-11-20","minutes":0}'); [ "$C" = "400" ] && ok "minuti a zero rifiutati" || no "minuti 0" "$C"
C=$(code T POST "/api/timesheets/$TS/lines" '{"activity":"x","work_date":"2026-11-20","minutes":2000}'); [ "$C" = "400" ] && ok "oltre 24 ore rifiutati" || no "minuti 2000" "$C"
C=$(code T POST "/api/timesheets/$TS/expenses" '{"description":"rimborso","amount_cents":-500}'); [ "$C" = "400" ] && ok "spesa negativa rifiutata" || no "spesa neg" "$C"
C=$(code T POST "/api/timesheets/$TS/lines" '{"activity":"x","work_date":"2026-11-20","minutes":30,"currency":"euro"}'); [ "$C" = "400" ] && ok "valuta non valida rifiutata" || no "valuta" "$C"
call T POST "/api/timesheets/$TS/lines" '{"activity":"Trasferta","work_date":"2026-11-20","minutes":30,"rate_cents":9999}' >/dev/null
[ "$(q "SELECT rate_cents FROM app.timesheet_lines WHERE timesheet_id='$TS' AND activity='Trasferta'")" = "3000" ] \
  && ok "il tecnico non sceglie la tariffa: vale il listino" || no "tariffa tecnico" "$(q "SELECT rate_cents FROM app.timesheet_lines WHERE timesheet_id='$TS' AND activity='Trasferta'")"
call T POST "/api/timesheets/$TS/expenses" '{"description":"Parcheggio","amount_cents":800}' >/dev/null
call T POST "/api/timesheets/$TS/lines" '{"activity":"Estero","work_date":"2026-11-20","minutes":60,"currency":"USD"}' >/dev/null
D=$(call T GET "/api/timesheets/$TS")
echo "$D" | py "d['total']['mixed_currency'] and 'USD' in d['total']['by_currency'] and d['total']['labor_cents']==(75*3000+30*3000+30)//60" | grep -q True \
  && ok "valute diverse: totale in EUR, USD a parte" || no "valute" "$(echo "$D" | py "d['total']")"

echo "== CA-8 correzione =="
call T POST "/api/timesheets/$TS/submit" '{}' >/dev/null
C=$(code T DELETE "/api/timesheets/$TS/expenses/$(echo "$D" | py "d['expenses'][0]['id']")"); [ "$C" = "409" ] && ok "in revisione: non si modifica (409)" || no "lock revisione" "$C"
call RS POST "/api/timesheets/$TS/request-changes" '{"note":"togli il parcheggio, la trasferta era di 45 minuti"}' >/dev/null
D=$(call T GET "/api/timesheets/$TS")
echo "$D" | py "d['state']=='da_correggere' and 'parcheggio' in d['review_note'] and 'guarnizione' in (d['note'] or '')" | grep -q True \
  && ok "nota del responsabile a parte; la nota con i materiali resta" || no "note" "$(echo "$D" | py "(d['review_note'],d['note'])")"
LID=$(echo "$D" | py "[l['id'] for l in d['lines'] if l['activity']=='Trasferta'][0]"); EID=$(echo "$D" | py "d['expenses'][0]['id']")
C=$(code T POST "/api/timesheets/$TS/lines/$LID" '{"activity":"Trasferta","work_date":"2026-11-20","minutes":45,"rate_cents":1}'); [ "$C" = "200" ] && ok "riga corretta" || no "modifica riga" "$C"
[ "$(q "SELECT minutes||'/'||rate_cents FROM app.timesheet_lines WHERE id='$LID'")" = "45/3000" ] && ok "minuti cambiati, tariffa invariata (non la sceglie il tecnico)" || no "riga" "$(q "SELECT minutes||'/'||rate_cents FROM app.timesheet_lines WHERE id='$LID'")"
C=$(code T DELETE "/api/timesheets/$TS/expenses/$EID"); [ "$C" = "200" ] && ok "spesa eliminata" || no "elimina spesa" "$C"
C=$(code RS POST "/api/timesheets/$TS/lines/$LID" '{"activity":"y","work_date":"2026-11-20","minutes":10}'); [ "$C" = "403" ] && ok "solo il proprietario corregge (403)" || no "altrui" "$C"
call T GET "/api/timesheets/$TS" | py "[e['detail'].get('note') for e in d['history'] if e['kind']=='request_changes'][0]" | grep -q "parcheggio" \
  && ok "lo storico conserva la nota del responsabile" || no "storico" "-"

echo "== tariffa fissata da chi approva =="
qq "UPDATE app.price_list_items SET active=false WHERE id='$IT'"
call T POST "/api/timesheets/$TS/lines" '{"activity":"Collaudo","work_date":"2026-11-20","minutes":30}' >/dev/null
L2=$(call T GET "/api/timesheets/$TS" | py "[l['id'] for l in d['lines'] if l['activity']=='Collaudo'][0]")
[ "$(q "SELECT rate_cents IS NULL FROM app.timesheet_lines WHERE id='$L2'")" = "t" ] && ok "senza listino la riga resta da valorizzare" || no "null rate" "-"
call T POST "/api/timesheets/$TS/submit" '{}' >/dev/null
C=$(code T POST "/api/timesheets/$TS/lines/$L2/rate" '{"rate_cents":5000}'); [ "$C" = "403" ] && ok "il tecnico non fissa la tariffa (403)" || no "rate tecnico" "$C"
C=$(code RS POST "/api/timesheets/$TS/lines/$L2/rate" '{"rate_cents":3500}'); [ "$C" = "200" ] && ok "chi approva fissa la tariffa in revisione" || no "rate resp" "$C"
C=$(code RS POST "/api/timesheets/$TS/approve" '{}'); [ "$C" = "200" ] && ok "approvato" || no "approva" "$C"

echo "timesheet-corrections: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
