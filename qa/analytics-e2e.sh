#!/usr/bin/env bash
# Fase 3 «Analisi e stime» (mig 0167, modulo analisi): durate reali, stima dallo storico con la sua
# base, precisione misurata sul passato, extra, ricorrenze dei piani, piano contro realtà, origine
# degli interventi. Tenant usa-e-getta con storico inserito a date note.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
sqlq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="an$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"An $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"analisi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
AT=$(call A POST /api/activity-types '{"name":"Taglio erba","unit":"m2","base_minutes":60,"minutes_per_unit":0,"crew_size":1}' | py "d['id']")
S1=$(call A POST /api/sites '{"name":"Parco grande"}' | py "d['id']"); S2=$(call A POST /api/sites '{"name":"Aiuola piccola"}' | py "d['id']")
CL=$(call A POST /api/clients '{"name":"Condominio Test"}' | py "d['id']")
# storico: ogni 7 giorni un taglio al Parco grande (120 min) e all'Aiuola piccola (60 min); 3 extra del cliente
for i in $(seq 1 6); do
  sqlq "INSERT INTO app.work_orders(tenant_id,title,state,site_id,activity_type_id,scheduled_start,scheduled_end,work_started_at,work_minutes,origin)
        VALUES('$TID','Taglio grande $i','completato','$S1','$AT',now()-interval '$((7*i)) days',now()-interval '$((7*i)) days'+interval '1 hour',now()-interval '$((7*i)) days',$((115+i*2)),'piano'),
              ('$TID','Taglio piccolo $i','completato','$S2','$AT',now()-interval '$((7*i+1)) days',now()-interval '$((7*i+1)) days'+interval '1 hour',now()-interval '$((7*i+1)) days',$((58+i)),'piano')"
done
for i in 1 2 3; do
  sqlq "INSERT INTO app.work_orders(tenant_id,title,state,site_id,client_id,scheduled_start,work_minutes,origin,is_extra)
        VALUES('$TID','Albero caduto $i','completato','$S1','$CL',now()-interval '$((10*i)) days',90,'manuale',true)"
done
PL=$(call A POST /api/maintenance-plans "{\"title\":\"Taglio parco\",\"site_id\":\"$S1\",\"every_days\":14,\"activity_type_id\":\"$AT\"}" | py "d['id']")

echo "== stima dallo storico =="
E=$(call A GET "/api/analytics/estimate?activity_type_id=$AT&site_id=$S1")
echo "$E" | py "(d['basis'],d['count'],110<=d['minutes']<=130)" | grep -q "('sede', 6, True)" && ok "stessa sede: mediana di 6 interventi (~120 min)" || no "stima sede" "$E"
echo "$E" | py "d['standard_minutes']" | grep -q "^60$" && ok "accanto la durata standard (60 min)" || no "standard" "$E"
S3=$(call A POST /api/sites '{"name":"Sede nuova"}' | py "d['id']")
call A GET "/api/analytics/estimate?activity_type_id=$AT&site_id=$S3" | py "(d['basis'],d['count'])" | grep -q "('tipo', 12)" && ok "sede nuova: tutte le sedi del tipo (12)" || no "stima tipo" "-"
CK=$(call A POST /api/orders/check "{\"activity_type_id\":\"$AT\",\"site_id\":\"$S1\"}")
echo "$CK" | py "d['history_estimate']['basis']" | grep -q sede && ok "il controllo dell'intervento propone la stima dallo storico" || no "check" "$CK"

echo "== durate, precisione, extra, ricorrenze, piano/realtà =="
D=$(call A GET '/api/analytics/durations?by=site')
echo "$D" | py "[r for r in d['rows'] if r['key']=='Parco grande'][0]['deviation_pct']>80" | grep -q True && ok "durate per sede: Parco grande quasi il doppio dello standard" || no "durate" "$D"
AC=$(call A GET '/api/analytics/accuracy?days=90')
echo "$AC" | py "d['count']>=10 and d['history_mae_minutes'] < d['standard_mae_minutes']" | grep -q True && ok "precisione: lo storico sbaglia meno della durata standard" || no "precisione" "$AC"
EX=$(call A GET '/api/analytics/extras?months=3')
echo "$EX" | py "(d['extra'], d['top_clients'][0]['name'])" | grep -q "(3, 'Condominio Test')" && ok "extra: 3, cliente che ne genera di più" || no "extra" "$EX"
RC=$(call A GET /api/analytics/recurrences)
echo "$RC" | py "[p for p in d['plans'] if p['plan_id']=='$PL'][0]['status']" | grep -q "più frequente" && ok "ricorrenze: ogni 7 giorni contro 14 a contratto" || no "ricorrenze" "$RC"
call A GET '/api/analytics/plan-vs-actual?weeks=8' | py "sum(w['completed'] for w in d['weeks'])>=10" | grep -q True && ok "piano contro realtà per settimana" || no "piano/realtà" "-"

echo "== origine ed extra =="
O=$(call A POST /api/orders '{"title":"Urgenza","is_extra":true}' | py "d['id']")
[ "$(q "SELECT origin||'/'||is_extra FROM app.work_orders WHERE id='$O'")" = "manuale/true" ] && ok "intervento extra creato a mano" || no "extra manuale" "-"
RQ=$(call A POST /api/requests '{"title":"Siepe danneggiata"}' | py "d['id']")
OR=$(call A POST "/api/requests/$RQ/convert" '{}' | py "d['id']")
[ "$(q "SELECT origin||'/'||is_extra FROM app.work_orders WHERE id='$OR'")" = "richiesta/true" ] && ok "da richiesta: origine «richiesta» ed extra" || no "richiesta" "-"
call A POST "/api/maintenance-plans/$PL/generate" '{}' >/dev/null
[ "$(q "SELECT origin FROM app.work_orders WHERE tenant_id='$TID' AND title LIKE 'Taglio parco%'")" = "piano" ] && ok "da piano: origine «piano»" || no "piano" "-"

echo "== modulo spento =="
sqlq "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='analisi'"
C=$(code A GET '/api/analytics/extras'); [ "$C" = "403" ] && ok "senza il modulo Analisi: 403" || no "modulo" "$C"
call A POST /api/orders/check "{\"activity_type_id\":\"$AT\",\"site_id\":\"$S1\"}" | py "d['history_estimate']" | grep -q None && ok "senza il modulo il controllo non propone la stima" || no "check senza modulo" "-"

echo "analytics: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
