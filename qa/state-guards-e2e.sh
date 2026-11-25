#!/usr/bin/env bash
# CA-13: macchine a stati con guardie (mig 0140).
#  - intervento completato: rapporto, firma, materiali, allegati e pianificazione non cambiano più
#  - richiesta di intervento: convertita una volta sola (anche con due richieste contemporanee);
#    una richiesta chiusa non si converte
#  - fatture: numero unico; emessa -> pagata | annullata, poi stato finale; una pagata non si
#    cancella; cambi di stato e cancellazioni nel registro attività
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sg$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Sg $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"cantieri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
UT=$(call A POST /api/users "{\"email\":\"tec@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']")
login T "tec@$SLUG.test" WorkPass9

echo "== intervento chiuso =="
O=$(call A POST /api/orders "{\"title\":\"Pompa $R\",\"assigned_user\":\"$UT\"}" | py "d['id']")
ver(){ q "SELECT version FROM app.work_orders WHERE id='$O'"; }
call T POST "/api/orders/$O/start" "{\"version\":$(ver)}" >/dev/null
call T POST "/api/orders/$O/report" '{"work_minutes":30,"signed_by":"Cliente Rossi"}' >/dev/null
call T POST "/api/orders/$O/complete" "{\"version\":$(ver),\"outcome\":\"ok\"}" >/dev/null
[ "$(q "SELECT state FROM app.work_orders WHERE id='$O'")" = "completato" ] && ok "intervento completato" || no "completa" "-"
C=$(code T POST "/api/orders/$O/report" '{"signed_by":"Altro firmatario","work_minutes":300}'); [ "$C" = "409" ] && ok "firma e rapporto di un intervento chiuso: 409" || no "report" "$C"
[ "$(q "SELECT signed_by FROM app.work_orders WHERE id='$O'")" = "ClienteRossi" ] && ok "la firma resta quella originale" || no "firma" "-"
C=$(code T POST "/api/orders/$O/lines" '{"description":"extra","qty":1}'); [ "$C" = "409" ] && ok "materiali su intervento chiuso: 409" || no "righe" "$C"
C=$(code A POST "/api/orders/$O/edit" "{\"version\":$(ver),\"title\":\"cambiato\"}"); [ "$C" = "409" ] && ok "pianificazione di un intervento chiuso: 409" || no "edit" "$C"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/T" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $(awk '$6=="yw_csrf"{print $7}' "$J/T" | tail -1)" -H 'content-type: image/png' -X POST "$BASE/api/orders/$O/attachments?kind=foto" --data-binary 'PNGDATA1234')
[ "$C" = "409" ] && ok "allegati su intervento chiuso: 409" || no "allegati" "$C"
O2=$(call A POST /api/orders "{\"title\":\"Aperto $R\",\"assigned_user\":\"$UT\"}" | py "d['id']")
C=$(code T POST "/api/orders/$O2/lines" '{"description":"guarnizione","qty":0}'); [ "$C" = "400" ] && ok "quantità zero rifiutata" || no "qty" "$C"
C=$(code T POST "/api/orders/$O2/lines" '{"description":"guarnizione","qty":1}'); [ "$C" = "200" ] && ok "intervento aperto: materiali ammessi" || no "righe aperto" "$C"

echo "== richieste =="
RQ=$(call A POST /api/requests '{"title":"Perdita acqua","priority":2}' | py "d['id']")
for i in 1 2 3; do code A POST "/api/requests/$RQ/convert" '{}' > "$J/c$i" & done; wait
N200=$(cat "$J/c1" "$J/c2" "$J/c3" | grep -o 200 | wc -l | tr -d " ")
[ "$N200" = "1" ] && ok "tre conversioni contemporanee: una sola riesce" || no "concorrenza" "$(cat "$J/c1" "$J/c2" "$J/c3")"
[ "$(q "SELECT count(*) FROM app.work_orders WHERE tenant_id='$TID' AND title='Perdita acqua'")" = "1" ] && ok "un solo ordine creato" || no "ordini" "-"
RQ2=$(call A POST /api/requests '{"title":"Chiusa","priority":2}' | py "d['id']")
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.service_requests SET status='chiusa' WHERE id='$RQ2'" >/dev/null
C=$(code A POST "/api/requests/$RQ2/convert" '{}'); [ "$C" = "409" ] && ok "richiesta chiusa: non si converte" || no "chiusa" "$C"

echo "== fatture =="
CM=$(call A POST /api/commesse "{\"code\":\"F$R\",\"name\":\"Fatture $R\"}" | py "d['id']")
call A POST "/api/commesse/$CM/fatture" '{"number":"FT-1","amount":1000}' >/dev/null
C=$(code A POST "/api/commesse/$CM/fatture" '{"number":"FT-1","amount":500}'); [ "$C" = "409" ] && ok "numero di fattura duplicato: 409" || no "dup" "$C"
C=$(code A POST "/api/commesse/$CM/fatture" '{"number":"FT-2","amount":-5}'); [ "$C" = "400" ] && ok "importo non positivo: 400" || no "importo" "$C"
F1=$(q "SELECT id FROM app.commessa_fatture WHERE tenant_id='$TID' AND number='FT-1'")
C=$(code A POST "/api/commessa-fatture/$F1" '{"status":"pagata"}'); [ "$C" = "200" ] && ok "emessa -> pagata" || no "pagata" "$C"
C=$(code A POST "/api/commessa-fatture/$F1" '{"status":"emessa"}'); [ "$C" = "409" ] && ok "pagata è finale: non torna emessa" || no "finale" "$C"
C=$(code A DELETE "/api/commessa-fatture/$F1"); [ "$C" = "409" ] && ok "fattura pagata: non si cancella" || no "delete pagata" "$C"
call A POST "/api/commesse/$CM/fatture" '{"number":"FT-3","amount":200}' >/dev/null
F3=$(q "SELECT id FROM app.commessa_fatture WHERE tenant_id='$TID' AND number='FT-3'")
C=$(code A DELETE "/api/commessa-fatture/$F3"); [ "$C" = "200" ] && ok "fattura emessa per errore: si cancella" || no "delete emessa" "$C"
[ "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND action IN ('commessa_fattura_stato','commessa_fattura_delete')")" = "2" ] \
  && ok "cambio di stato e cancellazione nel registro attività" || no "audit" "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND action LIKE 'commessa_fattura%'")"

echo "state-guards: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
