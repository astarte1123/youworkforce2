#!/usr/bin/env bash
# Fase 5 «Pianificazione automatica» (mig 0170, modulo pianificazione): impostazioni, motore di base
# (patentino → squadra giusta, mezzo prenotato, finestra della sede, giorno di pioggia evitato, sede
# senza posizione non pianificata col motivo, nessuna sovrapposizione), contratto dati, applicazione
# parziale, intervento cambiato dopo la proposta, intervento fissato, proposta esterna validata
# (sovrapposizione e patentino = vincoli duri), permessi. Tenant usa-e-getta, settimana futura fissa.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sqlq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="pl$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Pl $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"qualifiche\",\"mezzi\",\"meteo\",\"pianificazione\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A PUT /api/settings/timezone '{"timezone":"Europe/Rome"}' >/dev/null
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"$2\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
W1=$(mk w1 lavoratore); W2=$(mk w2 lavoratore); W3=$(mk w3 lavoratore); mk pl pianificatore >/dev/null
login W "w1@$SLUG.test" WorkPass9; login P "pl@$SLUG.test" WorkPass9
QP=$(call A POST /api/qualifications '{"name":"Abilitazione PLE"}' | py "d['id']")
call A POST "/api/users/$W1/qualifications" "{\"qualification_id\":\"$QP\",\"expires_at\":\"2030-01-01T00:00:00Z\"}" >/dev/null
G1=$(call A POST /api/user-groups '{"name":"Squadra Alta","kind":"squadra"}' | py "d['id']")
G2=$(call A POST /api/user-groups '{"name":"Squadra Bassa","kind":"squadra"}' | py "d['id']")
call A POST "/api/user-groups/$G1/members" "{\"user_id\":\"$W1\"}" >/dev/null
for u in $W2 $W3; do call A POST "/api/user-groups/$G2/members" "{\"user_id\":\"$u\"}" >/dev/null; done
DEP=$(call A POST /api/sites '{"name":"Deposito","lat":45.503,"lon":9.129}' | py "d['id']")
SA=$(call A POST /api/sites '{"name":"Parco A","lat":45.464,"lon":9.190}' | py "d['id']")
SB=$(call A POST /api/sites '{"name":"Parco B","lat":45.470,"lon":9.200}' | py "d['id']")
SCF=$(call A POST /api/sites '{"name":"Villa Monza","lat":45.584,"lon":9.274}' | py "d['id']")
SD=$(call A POST /api/sites '{"name":"Scuola","lat":45.480,"lon":9.160,"access_windows":[{"days":"1111100","from":"09:00","to":"11:00"}]}' | py "d['id']")
SE=$(call A POST /api/sites '{"name":"Senza posizione"}' | py "d['id']")
ATP=$(call A POST /api/activity-types "{\"name\":\"Potatura in quota\",\"base_minutes\":120,\"crew_size\":1,\"qualification_ids\":[\"$QP\"],\"assets\":[{\"kind\":\"piattaforma\",\"qty\":1}]}" | py "d['id']")
ATT=$(call A POST /api/activity-types '{"name":"Taglio erba","base_minutes":90,"crew_size":2,"weather":{"rain_max_mm":2}}' | py "d['id']")
PF=$(call A POST /api/assets "{\"name\":\"Piattaforma 18 m\",\"kind\":\"piattaforma\",\"required_qualification_id\":\"$QP\"}" | py "d['id']")
# lunedì 1/3/2027 piove forte su Parco A e Parco B (zona Milano), il resto della settimana è asciutto
call A PUT /api/integration/weather/zones/MI '{"name":"Milano","lat":45.465,"lon":9.19}' >/dev/null
call A POST /api/integration/weather/slots '{"zone":"MI","slots":[{"from":"2027-03-01T05:00:00Z","to":"2027-03-01T17:00:00Z","rain_mm":30},{"from":"2027-03-02T05:00:00Z","to":"2027-03-02T17:00:00Z","rain_mm":0},{"from":"2027-03-03T05:00:00Z","to":"2027-03-03T17:00:00Z","rain_mm":0},{"from":"2027-03-04T05:00:00Z","to":"2027-03-04T17:00:00Z","rain_mm":0},{"from":"2027-03-05T05:00:00Z","to":"2027-03-05T17:00:00Z","rain_mm":0}]}' | py "d['slots']" | grep -q "^5$" || no "previsione di prova" "-"
O(){ call A POST /api/orders "{\"title\":\"$1\",\"site_id\":\"$2\",\"activity_type_id\":\"$3\"${4:+,$4}}" | py "d['id']"; }
O1=$(O "Potatura tigli" $SA $ATP); O2=$(O "Taglio parco A" $SA $ATT); O3=$(O "Taglio parco B" $SB $ATT)
O4=$(O "Taglio villa" $SCF $ATT); O5=$(O "Taglio scuola" $SD $ATT); O6=$(O "Taglio senza posizione" $SE $ATT); O7=$(O "Potatura villa" $SCF $ATP)
FROM="2027-02-28T23:00:00Z"; TO="2027-03-05T23:00:00Z"

echo "== impostazioni =="
C=$(code A PUT /api/planning/settings '{"day_start":"17:00","day_end":"08:00","workdays":"1111100","travel_kmh":25,"detour_factor":1.3,"setup_minutes":5,"max_overtime_minutes":0,"weather_policy":"evita"}')
[ "$C" = "400" ] && ok "giornata che finisce prima di iniziare: 400" || no "validazione" "$C"
C=$(code W PUT /api/planning/settings '{"day_start":"07:30","day_end":"16:30","workdays":"1111100","travel_kmh":25,"detour_factor":1.3,"setup_minutes":5,"max_overtime_minutes":0,"weather_policy":"evita"}')
[ "$C" = "403" ] && ok "un lavoratore non cambia le impostazioni" || no "permesso impostazioni" "$C"
call A PUT /api/planning/settings "{\"depot_site_id\":\"$DEP\",\"day_start\":\"07:30\",\"day_end\":\"16:30\",\"workdays\":\"1111100\",\"travel_kmh\":25,\"detour_factor\":1.3,\"setup_minutes\":5,\"max_overtime_minutes\":0,\"weather_policy\":\"evita\"}" | py "d['ok']" | grep -q True && ok "deposito e giornata salvati" || no "impostazioni" "-"
C=$(code W POST /api/planning/runs "{\"from\":\"$FROM\",\"to\":\"$TO\"}"); [ "$C" = "403" ] && ok "un lavoratore non lancia la pianificazione" || no "permesso run" "$C"

echo "== proposta del motore di base =="
RUN=$(call P POST /api/planning/runs "{\"from\":\"$FROM\",\"to\":\"$TO\"}")
RID=$(echo "$RUN" | py "d['id']")
echo "$RUN" | py "(d['status'], d['summary']['orders'], d['summary']['planned'], d['summary']['hard'])" | grep -q "('bozza', 7, 6, 0)" && ok "bozza: 6 di 7 pianificati, nessun vincolo violato" || no "riepilogo" "$(echo "$RUN" | py "d['summary']")"
IT(){ echo "$RUN" | py "[i for i in d['items'] if i['order_id']=='$1'][0]$2"; }
IT $O6 "['reason']" | grep -q "posizione" && ok "sede senza posizione: non pianificato, col motivo" || no "motivo" "$(IT $O6 "")"
IT $O1 "['team'], [i for i in d['items'] if i['order_id']=='$O1'][0]['assets']" | grep -q "Squadra Alta.*Piattaforma 18 m" && ok "potatura in quota alla squadra con la PLE, piattaforma prenotata" || no "patentino/mezzo" "$(IT $O1 "")"
IT $O4 "['team']" | grep -q "Squadra Bassa" && ok "taglio con 2 persone alla squadra che le ha" || no "persone" "$(IT $O4 "")"
[ "$(IT $O2 "['start'][:10]")" != "2027-03-01" ] && [ "$(IT $O3 "['start'][:10]")" != "2027-03-01" ] && ok "tagli dei parchi A e B non nel lunedì di pioggia" || no "pioggia" "$(IT $O2 "['start']") $(IT $O3 "['start']")"
python -c "import datetime as t;s=t.datetime.fromisoformat('$(IT $O5 "['start']")'.replace('Z','+00:00'));e=t.datetime.fromisoformat('$(IT $O5 "['end']")'.replace('Z','+00:00'));print(s.hour+1>=9 and (e.hour+1, e.minute)<=(11,0))" | grep -q True \
  && ok "scuola nella finestra 09:00–11:00" || no "finestra" "$(IT $O5 "")"
echo "$RUN" | python -c "
import sys,json;d=json.load(sys.stdin);it=[i for i in d['items'] if i['start']]
bad=[(a['title'],b['title']) for a in it for b in it if a is not b and a['group_id']==b['group_id'] and a['start']<b['end'] and b['start']<a['end']]
print(len(bad)==0 and all(i['travel_minutes'] is not None and i['seq'] for i in it))" | grep -q True && ok "nessuna sovrapposizione nella stessa squadra; ordine e spostamenti per ogni visita" || no "sovrapposizioni" "-"
echo "$RUN" | py "d['summary']['travel_km']>0 and d['summary']['current_travel_km']==0" | grep -q True && ok "spostamenti stimati (prima nessun piano)" || no "spostamenti" "-"
echo "$RUN" | py "sum(i['travel_km'] or 0 for i in d['items'] if i['start'])>5" | grep -q True && ok "chilometri per visita nella bozza" || no "km per visita" "-"
C=$(code P POST /api/planning/backtest "{\"from\":\"$FROM\",\"to\":\"$TO\"}"); [ "$C" = "400" ] && ok "prova sullo storico solo su periodi passati" || no "backtest futuro" "$C"
PR=$(call P GET "/api/planning/problem?from=$FROM&to=$TO")
echo "$PR" | py "(d['contract'], len(d['orders']), len(d['teams']), [o['weather_bad_days'] for o in d['orders'] if o['id']=='$O2'][0])" | grep -q "('yw-planning/1', 7, 2, \['2027-03-01'\])" \
  && ok "contratto dati: interventi, squadre e giorni di meteo sfavorevole" || no "contratto" "$(echo "$PR" | head -c 400)"

echo "== applicazione =="
V3=$(call P GET "/api/orders?id=$O3" | py "d[0]['version']")
call P POST /api/orders/$O3/edit "{\"version\":$V3,\"title\":\"Taglio parco B (rinominato)\"}" >/dev/null
AP=$(call P POST /api/planning/runs/$RID/apply "{\"order_ids\":[\"$O1\",\"$O2\",\"$O3\"]}")
echo "$AP" | py "(d['applied'], len(d['errors']))" | grep -q "(2, 1)" && ok "applicati 2, uno no" || no "applica" "$AP"
echo "$AP" | py "d['errors'][0]['error']" | grep -q "modificato dopo la proposta" && ok "intervento cambiato dopo la proposta: non applicato" || no "stale" "$AP"
OD=$(call P GET "/api/orders?id=$O1")
echo "$OD" | py "(d[0]['group_name'], d[0]['state'], bool(d[0]['scheduled_start']), [a['name'] for a in d[0]['assets']])" | grep -q "('Squadra Alta', 'assegnato', True, \['Piattaforma 18 m'\])" \
  && ok "intervento in agenda: squadra, orario, stato assegnato, piattaforma prenotata" || no "agenda" "$OD"
C=$(code P DELETE /api/planning/runs/$RID); [ "$C" = "409" ] && ok "una proposta applicata non si scarta" || no "scarta applicata" "$C"

echo "== intervento fissato e ripianifica =="
call P POST /api/orders/$O2/plan-lock '{"locked":true}' | py "d['locked']" | grep -q True && ok "intervento fissato" || no "fissa" "-"
R2=$(call P POST /api/planning/runs "{\"from\":\"$FROM\",\"to\":\"$TO\",\"replan\":true}")
echo "$R2" | py "('$O2' not in [i['order_id'] for i in d['items']], d['summary']['fixed']>=1)" | grep -q "(True, True)" && ok "ripianifica: l'intervento fissato resta dov'è" || no "fissato" "$(echo "$R2" | py "d['summary']")"
R2ID=$(echo "$R2" | py "d['id']")
call P DELETE /api/planning/runs/$R2ID | py "d['ok']" | grep -q True && ok "bozza scartata" || no "scarta" "-"
R3=$(call P POST /api/planning/runs "{\"from\":\"$FROM\",\"to\":\"$TO\",\"replan\":true,\"group_ids\":[\"$G2\"]}")
echo "$R3" | py "('$O1' not in [i['order_id'] for i in d['items']], all(i['group_id'] in (None,'$G2') for i in d['items']))" | grep -q "(True, True)"   && ok "solo una squadra: gli interventi delle altre squadre non si toccano" || no "filtro squadre" "$(echo "$R3" | py "d['summary']")"
call P DELETE /api/planning/runs/$(echo "$R3" | py "d['id']") >/dev/null

echo "== proposta di un motore esterno =="
D3="2027-03-03"
EX=$(call P POST /api/planning/runs/external "{\"from\":\"$FROM\",\"to\":\"$TO\",\"engine\":\"Youco solver (prova)\",\"items\":[
 {\"order_id\":\"$O4\",\"group_id\":\"$G2\",\"start\":\"${D3}T08:00:00Z\",\"end\":\"${D3}T09:30:00Z\"},
 {\"order_id\":\"$O5\",\"group_id\":\"$G2\",\"start\":\"${D3}T09:00:00Z\",\"end\":\"${D3}T09:45:00Z\"},
 {\"order_id\":\"$O7\",\"group_id\":\"$G2\",\"start\":\"${D3}T12:00:00Z\",\"end\":\"${D3}T14:00:00Z\"},
 {\"order_id\":\"$O6\",\"reason\":\"manca la posizione\"}]}")
echo "$EX" | py "(d['source'], d['engine'], d['summary']['hard'])" | grep -q "('esterno', 'Youco solver (prova)', 3)" && ok "proposta esterna validata da YW: 3 voci con vincoli violati" || no "esterna" "$(echo "$EX" | py "d['summary']")"
echo "$EX" | py "[v['code'] for i in d['items'] if i['order_id']=='$O7' for v in i['violations'] if v['hard']]" | grep -q "qualification" && ok "patentino mancante segnalato" || no "patentino" "-"
echo "$EX" | py "[v['code'] for i in d['items'] if i['order_id']=='$O4' for v in i['violations']]" | grep -q "plan_overlap" && ok "sovrapposizione nella proposta segnalata" || no "overlap" "-"
EXID=$(echo "$EX" | py "d['id']")
call P POST /api/planning/runs/$EXID/apply "{\"order_ids\":[\"$O7\"]}" | py "(d['applied'], len(d['errors']))" | grep -q "(0, 1)" && ok "vincolo duro: non applicato senza forzare" || no "duro" "-"
C=$(code P POST /api/planning/runs/external "{\"from\":\"$FROM\",\"to\":\"$TO\",\"engine\":\"x\",\"items\":[{\"order_id\":\"$O4\",\"start\":\"${D3}T08:00:00Z\",\"end\":\"${D3}T09:00:00Z\"}]}")
[ "$C" = "400" ] && ok "voce senza squadra né persona: 400" || no "voce" "$C"
call P GET /api/planning/runs | py "len(d)>=3" | grep -q True && ok "elenco delle proposte" || no "elenco" "-"

echo "== prova sullo storico =="
PM=$(python -c "import datetime as t;d=t.date.today();d-=t.timedelta(days=d.weekday()+7);print(d)")
for k in 0 1 2; do for site in $SA $SCF $SB; do
  sqlq "INSERT INTO app.work_orders(tenant_id,title,state,site_id,activity_type_id,assigned_group,scheduled_start,scheduled_end,work_minutes)
        VALUES('$TID','Storico $k','completato','$site','$ATT','$G2','${PM}T08:00:00Z'::timestamptz+interval '$k day'+interval '$((RANDOM%3)) hour','${PM}T09:30:00Z'::timestamptz+interval '$k day'+interval '3 hour',95)"
done; done
BT=$(call P POST /api/planning/backtest "{\"from\":\"$(python -c "print('${PM}T00:00:00Z')")\",\"to\":\"$(python -c "import datetime as t;print(str(t.date.fromisoformat('$PM')+t.timedelta(days=5))+'T00:00:00Z')")\"}")
echo "$BT" | py "(d['orders'], d['hard'], 'current_hard' in d, d['current_travel_km']>0, d['travel_km']<=d['current_travel_km'])" | grep -q "(9, 0, True, True, True)"   && ok "storico: 9 interventi ripianificati, nessun vincolo violato, spostamenti non peggiori del piano a mano" || no "backtest" "$BT"
call P GET /api/planning/runs | py "len(d)" > "$J/n1"; [ "$(cat "$J/n1")" -ge 3 ] && ok "la prova sullo storico non salva proposte" || no "backtest salva" "-"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
