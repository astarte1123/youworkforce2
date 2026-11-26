#!/usr/bin/env bash
# Fase 6 «Pilota» della pianificazione automatica (mig 0171): squadre, settimane prima/durante,
# misure (accettati senza modifiche, spostamenti per intervento, straordinario, extra entro lo SLA del
# contratto, ore di pianificazione risparmiate), obiettivi, un solo pilota attivo, decisione finale,
# proposte limitate alle squadre del pilota, permessi. Dati inseriti a date note rispetto a oggi.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sqlq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="pi$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Pi $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"pianificazione\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A PUT /api/settings/timezone '{"timezone":"Europe/Rome"}' >/dev/null
call A POST /api/users "{\"email\":\"w@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" >/dev/null; login W "w@$SLUG.test" WorkPass9
G1=$(call A POST /api/user-groups '{"name":"Squadra Uno","kind":"squadra"}' | py "d['id']")
G2=$(call A POST /api/user-groups '{"name":"Squadra Due","kind":"squadra"}' | py "d['id']")
GR=$(call A POST /api/user-groups '{"name":"Ufficio","kind":"reparto"}' | py "d['id']")
DEP=$(call A POST /api/sites '{"name":"Deposito","lat":45.503,"lon":9.129}' | py "d['id']")
SN=$(call A POST /api/sites '{"name":"Nord","lat":45.55,"lon":9.22}' | py "d['id']")
SS=$(call A POST /api/sites '{"name":"Sud","lat":45.40,"lon":9.15}' | py "d['id']")
SN2=$(call A POST /api/sites '{"name":"Nord 2","lat":45.555,"lon":9.225}' | py "d['id']")
SS2=$(call A POST /api/sites '{"name":"Sud 2","lat":45.405,"lon":9.155}' | py "d['id']")
call A PUT /api/planning/settings "{\"depot_site_id\":\"$DEP\",\"day_start\":\"07:30\",\"day_end\":\"16:30\",\"workdays\":\"1111100\",\"travel_kmh\":25,\"detour_factor\":1.3,\"setup_minutes\":5,\"max_overtime_minutes\":0,\"weather_policy\":\"evita\"}" >/dev/null
CL=$(call A POST /api/clients '{"name":"Condominio SLA"}' | py "d['id']")
call A POST /api/contracts "{\"name\":\"Manutenzione 2026\",\"client_id\":\"$CL\",\"sla_hours\":24}" >/dev/null
# settimane: pilota dal lunedì scorso (settimana 1 svolta, settimana 2 = questa), confronto con le 2 prima
read P0 B1 B2 <<<"$(python -c "import datetime as t;d=t.date.today();m=d-t.timedelta(days=d.weekday()+7);print(m, m-t.timedelta(days=14), m-t.timedelta(days=7))")"
ord(){ # ord <giorno> <ora-utc> <sede> <squadra> -> id (completato)
  docker exec -i ywdb psql -U postgres -d youworkforce -tAc "INSERT INTO app.work_orders(tenant_id,title,state,site_id,assigned_group,scheduled_start,scheduled_end)
    VALUES('$TID','Lavoro','completato','$3','$4','$1T$2:00:00Z','$1T$2:00:00Z'::timestamptz+interval '90 minutes') RETURNING id" | head -1 | tr -d '[:space:]'; }
day(){ python -c "import datetime as t;print(t.date.fromisoformat('$1')+t.timedelta(days=$2))"; }
# prima: Nord e Sud alternati (tanti spostamenti); squadra 2 fuori dal pilota
for W in $B1 $B2; do D=$(day $W 1)
  ord $D 07 $SN $G1 >/dev/null; ord $D 09 $SS $G1 >/dev/null; ord $D 11 $SN2 $G1 >/dev/null; ord $D 13 $SS2 $G1 >/dev/null; ord $D 08 $SN $G2 >/dev/null; done
# pilota settimana 1: Nord con Nord, Sud con Sud, da una proposta applicata (una spostata a mano dopo)
D=$(day $P0 1)
A1=$(ord $D 07 $SN $G1); A2=$(ord $D 09 $SN2 $G1); A3=$(ord $(day $P0 2) 07 $SS $G1); A4=$(ord $(day $P0 2) 11 $SS2 $G1)
RUN=$(q "INSERT INTO app.planning_runs(tenant_id,engine,status,period_from,period_to,applied_at) VALUES('$TID','yw-base-1','applicato','${P0}T00:00:00Z','$(day $P0 5)T00:00:00Z',now()) RETURNING id")
for o in $A1 $A2 $A3; do sqlq "INSERT INTO app.planning_run_items(run_id,tenant_id,work_order_id,order_version,group_id,starts_at,ends_at,applied)
  SELECT '$RUN','$TID',id,version,assigned_group,scheduled_start,scheduled_end,true FROM app.work_orders WHERE id='$o'"; done
sqlq "INSERT INTO app.planning_run_items(run_id,tenant_id,work_order_id,order_version,group_id,starts_at,ends_at,applied)
  SELECT '$RUN','$TID',id,version,assigned_group,scheduled_start - interval '2 hours',scheduled_end - interval '2 hours',true FROM app.work_orders WHERE id='$A4'"
# richieste extra: una servita in 7 ore (entro 24), una dopo 2 giorni (fuori SLA)
sqlq "INSERT INTO app.service_requests(tenant_id,client_id,title,status,order_id,created_at) VALUES('$TID','$CL','Ramo caduto','convertita','$A1','${D}T06:00:00Z'),('$TID','$CL','Siepe','convertita','$A3','${D}T06:00:00Z')"
sqlq "UPDATE app.work_orders SET arrived_at='${D}T13:00:00Z' WHERE id='$A1'; UPDATE app.work_orders SET arrived_at=scheduled_start + interval '1 day' WHERE id='$A3'"
PIL(){ echo "{\"name\":\"Pilota prova\",\"group_ids\":[$1],\"starts_on\":\"$P0\",\"weeks\":2,\"baseline_weeks\":2,\"baseline_planning_hours\":10,\"targets\":{\"accepted_pct\":70,\"travel_reduction_pct\":10,\"sla_pct\":90,\"hours_saved\":4}}"; }

echo "== avvio =="
call A GET /api/planning/pilot | py "d['pilot']" | grep -q "^None$" && ok "nessun pilota all'inizio" || no "vuoto" "-"
C=$(code A POST /api/planning/pilot "$(PIL "")"); [ "$C" = "400" ] && ok "senza squadre: 400" || no "squadre" "$C"
C=$(code A POST /api/planning/pilot "$(PIL "\"$GR\"")"); [ "$C" = "400" ] && ok "un reparto non è una squadra: 400" || no "reparto" "$C"
C=$(code W POST /api/planning/pilot "$(PIL "\"$G1\"")"); [ "$C" = "403" ] && ok "un lavoratore non avvia il pilota" || no "permesso" "$C"
PJ=$(call A POST /api/planning/pilot "$(PIL "\"$G1\"")"); PID=$(echo "$PJ" | py "d['pilot']['id']")
echo "$PJ" | py "(d['pilot']['status'], d['pilot']['teams'], d['pilot']['current_week'], len(d['pilot']['weeks_detail']))" | grep -q "('attivo', \['Squadra Uno'\], 2, 4)" \
  && ok "pilota attivo: squadra, settimana 2 di 2, 2 settimane prima + 2 durante" || no "avvio" "$(echo "$PJ" | head -c 300)"
C=$(code A POST /api/planning/pilot "$(PIL "\"$G2\"")"); [ "$C" = "409" ] && ok "un solo pilota attivo per volta" || no "uno solo" "$C"

echo "== misure =="
M(){ echo "$PJ" | py "[m for m in d['pilot']['measures'] if m['code']=='$1'][0]$2"; }
W(){ echo "$PJ" | py "d['pilot']['weeks_detail'][$1]$2"; }
[ "$(W 0 "['orders']")" = "4" ] && ok "settimana prima: solo gli interventi della squadra del pilota (4 su 5)" || no "squadra fuori" "$(W 0 "")"
M accepted "['pilot'], [m for m in d['pilot']['measures'] if m['code']=='accepted'][0]['ok']" | grep -q "^75.0 True$" && ok "3 interventi su 4 rimasti come proposti: 75%, obiettivo 70% raggiunto" || no "accettati" "$(M accepted "")"
M travel "['change']>10 and [m for m in d['pilot']['measures'] if m['code']=='travel'][0]['ok']" | grep -q True && ok "spostamenti per intervento: oltre il 10% in meno" || no "spostamenti" "$(M travel "")"
M sla "['pilot'], [m for m in d['pilot']['measures'] if m['code']=='sla'][0]['ok']" | grep -q "^50.0 False$" && ok "extra: 1 su 2 entro lo SLA di 24 ore del contratto" || no "sla" "$(M sla "")"
M overtime "['baseline'] is not None" | grep -q True && ok "straordinario prima e durante" || no "straordinario" "$(M overtime "")"
M hours "['ok']" | grep -q None && ok "ore di pianificazione: senza dichiarazioni nessun esito" || no "ore vuote" "$(M hours "")"

echo "== ore di pianificazione =="
C=$(code A POST /api/planning/pilot/$PID/weeks "{\"week_start\":\"$B1\",\"planning_hours\":3}"); [ "$C" = "400" ] && ok "settimana fuori dal pilota: 400" || no "fuori" "$C"
call A POST /api/planning/pilot/$PID/weeks "{\"week_start\":\"$(day $P0 3)\",\"planning_hours\":6,\"note\":\"prima settimana\"}" | py "d['ok']" | grep -q True && ok "ore della settimana 1 (data qualsiasi della settimana)" || no "ore" "-"
PJ=$(call A GET /api/planning/pilot)
M hours "['change'], [m for m in d['pilot']['measures'] if m['code']=='hours'][0]['ok']" | grep -q "^4.0 True$" && ok "10 ore prima, 6 durante: 4 risparmiate, obiettivo raggiunto" || no "risparmio" "$(M hours "")"

echo "== proposte limitate al pilota =="
NX=$(python -c "import datetime as t;d=t.date.today();m=d+t.timedelta(days=7-d.weekday());print(m)")
RJ=$(call A POST /api/planning/runs "{\"from\":\"${NX}T00:00:00Z\",\"to\":\"$(day $NX 5)T00:00:00Z\",\"group_ids\":[\"$G1\"]}")
echo "$RJ" | py "d['summary']['teams']" | grep -q "^1$" && ok "la proposta considera solo la squadra del pilota" || no "squadre run" "$(echo "$RJ" | head -c 200)"

echo "== decisione =="
C=$(code A POST /api/planning/pilot/$PID/close '{"decision":"forse"}'); [ "$C" = "400" ] && ok "decisione non valida: 400" || no "decisione" "$C"
call A POST /api/planning/pilot/$PID/close '{"decision":"estendi","note":"obiettivi raggiunti tranne lo SLA"}' | py "(d['pilot']['status'], d['pilot']['decision_note'])" | grep -q "('esteso', 'obiettivi raggiunti tranne lo SLA')" \
  && ok "esteso a tutte le squadre, con il motivo" || no "estendi" "-"
C=$(code A POST /api/planning/pilot/$PID/close '{"decision":"concludi"}'); [ "$C" = "409" ] && ok "un pilota chiuso non si richiude" || no "richiudi" "$C"
C=$(code A POST /api/planning/pilot "$(PIL "\"$G2\"")"); [ "$C" = "200" ] && ok "chiuso il primo, se ne può avviare un altro" || no "nuovo" "$C"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
