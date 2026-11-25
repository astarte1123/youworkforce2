#!/usr/bin/env bash
# CA-12 + CA-16 (mig 0135): ronda pianificata non svolta -> incidente «Ronda mancata»; orari nel
# fuso locale (tenant, sovrascrivibile dalla sede).
#  - tenant con SOLO il modulo Ronde: la Centrale è disponibile
#  - fuso del tenant e della sede: validati e salvati
#  - slot nel fuso locale: «HH:MM» della pianificazione = ora locale, non UTC
#  - slot scaduto senza ronda: un incidente 'ronda_mancata', attribuito alla sede del giro, uno solo
#  - slot coperto da una ronda avviata in tempo: nessun incidente
#  - cruscotto: slot mancato con collegamento all'incidente
# Tenant usa-e-getta. Richiede TEST_CLOCK=1.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
NOW=$(curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | py "d.get('now','')" 2>/dev/null)
[ -n "$NOW" ] || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="pm$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Pm $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"giri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
nmiss(){ q "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND etype='ronda_mancata'"; }
wait_wd(){ for i in $(seq 1 34); do sleep 1; done; }   # il controllo gira ogni ~30 s

echo "== solo Ronde: Centrale disponibile =="
C=$(code A GET /api/incidents); [ "$C" = "200" ] && ok "tenant con solo Ronde: Centrale allarmi accessibile" || no "centrale" "$C"

echo "== fuso orario =="
C=$(code A PUT /api/settings/timezone '{"timezone":"Marte/Olimpo"}'); [ "$C" = "400" ] && ok "fuso sconosciuto rifiutato" || no "tz invalido" "$C"
C=$(code A PUT /api/settings/timezone '{"timezone":"Europe/Rome"}'); [ "$C" = "200" ] && ok "fuso del tenant salvato (Europe/Rome)" || no "tz" "$C"
C=$(code A POST /api/sites '{"name":"X","timezone":"Nowhere/City"}'); [ "$C" = "400" ] && ok "fuso della sede validato" || no "tz sede" "$C"
SITE=$(call A POST /api/sites "{\"name\":\"Deposito $R\",\"timezone\":\"Europe/Rome\"}" | py "d['id']")
[ "$(q "SELECT timezone FROM app.sites WHERE id='$SITE'")" = "Europe/Rome" ] && ok "fuso della sede salvato" || no "tz sede salvata" "-"

# orari: slot a «adesso + 2 min» ora LOCALE, ogni 5 min, due slot
read FROM TO SLOT_UTC ADVS COVER_FROM COVER_TO <<<"$(python - "$NOW" <<'EOF'
import sys, datetime as dt
from zoneinfo import ZoneInfo
now = dt.datetime.fromisoformat(sys.argv[1].replace('Z','+00:00')[:26]+'+00:00') if '+' not in sys.argv[1][19:] else dt.datetime.fromisoformat(sys.argv[1])
loc = now.astimezone(ZoneInfo('Europe/Rome')).replace(second=0, microsecond=0)
f = loc + dt.timedelta(minutes=2); t = f + dt.timedelta(minutes=5)
cf = loc + dt.timedelta(minutes=1); ct = cf
adv = int(((f + dt.timedelta(minutes=5)) - now).total_seconds()) + 60
print(f.strftime('%H:%M'), t.strftime('%H:%M'), f.astimezone(dt.timezone.utc).strftime('%H:%M'), adv, cf.strftime('%H:%M'), ct.strftime('%H:%M'))
EOF
)"
RT=$(call A POST /api/patrol-routes "{\"name\":\"Notte $R\",\"site_id\":\"$SITE\"}" | py "d['id']")
call A POST "/api/patrol-routes/$RT/checkpoints" '{"name":"Cancello","kind":"qr"}' >/dev/null
call A POST /api/patrol-schedules "{\"route_id\":\"$RT\",\"from_time\":\"$FROM\",\"to_time\":\"$TO\",\"every_minutes\":5}" >/dev/null
RT2=$(call A POST /api/patrol-routes "{\"name\":\"Svolta $R\",\"site_id\":\"$SITE\"}" | py "d['id']")
call A POST "/api/patrol-routes/$RT2/checkpoints" '{"name":"Porta","kind":"qr"}' >/dev/null
call A POST /api/patrol-schedules "{\"route_id\":\"$RT2\",\"from_time\":\"$COVER_FROM\",\"to_time\":\"$COVER_TO\",\"every_minutes\":5}" >/dev/null

echo "== fuso locale degli slot =="
D=$(call A GET /api/patrol-dashboard)
echo "$D" | py "[s for s in d['slots'] if s['route_name']=='Notte $R'][0]['local_time']" | grep -q "^$FROM$" \
  && ok "slot alle $FROM ora locale (non UTC)" || no "local_time" "$(echo "$D" | head -c 300)"
echo "$D" | py "[s for s in d['slots'] if s['route_name']=='Notte $R'][0]['at'][11:16]" | grep -q "^$SLOT_UTC$" \
  && ok "lo stesso slot in UTC è alle $SLOT_UTC" || no "utc" "-"

echo "== ronda svolta in tempo =="
RDJ=$(call A POST "/api/patrol-routes/$RT2/rounds" '{}')   # avviata ora: copre lo slot $COVER_FROM
RD=$(echo "$RDJ" | py "d.get('id','')" 2>/dev/null)
# diagnosi del fallimento intermittente in run-all: se l'avvio non riesce lo si vede subito
[ -n "$RD" ] && ok "ronda del giro «Svolta» avviata" || no "avvio ronda" "$RDJ"
# l'avvio è registrato con l'orologio del DB: lo allineo all'orologio di test (indipendente da avanzamenti precedenti)
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.patrol_rounds SET started_at='$NOW' WHERE id='$RD'"
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.patrol_schedules SET created_at='$NOW'::timestamptz - interval '1 minute' WHERE tenant_id='$TID'"

echo "== ronda mancata =="
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$ADVS}"
wait_wd
[ "$(nmiss)" = "1" ] && ok "slot scaduto senza ronda: un incidente «Ronda mancata»" || no "incidente" "$(nmiss)"
[ "$(q "SELECT count(*) FROM app.incidents i JOIN app.patrol_missed m ON m.incident_id=i.id WHERE i.tenant_id='$TID' AND i.site_id='$SITE' AND m.route_id='$RT'")" = "1" ] \
  && ok "incidente collegato allo slot e attribuito alla sede del giro" || no "sede/collegamento" "-"
[ "$(q "SELECT count(*) FROM app.patrol_missed WHERE tenant_id='$TID' AND route_id='$RT2'")" = "0" ] \
  && ok "giro svolto in tempo: nessun allarme" || no "falso allarme" "$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT 'slot='||m.slot_at||' deadline='||m.deadline_at||' ronde='||(SELECT string_agg(r.started_at::text||'/'||r.status,',') FROM app.patrol_rounds r WHERE r.route_id='$RT2')||' sched='||(SELECT string_agg(from_time||'-'||to_time||'/'||every_minutes||' created '||created_at,',') FROM app.patrol_schedules WHERE route_id='$RT2')||' NOW=$NOW' FROM app.patrol_missed m WHERE m.route_id='$RT2'" | tr '\n' ' ')"
wait_wd
[ "$(nmiss)" = "1" ] && ok "nessun doppione al controllo successivo" || no "doppio" "$(nmiss)"
call A GET /api/incidents | py "len([i for i in (d if isinstance(d,list) else d.get('incidents',[])) if i.get('etype')=='ronda_mancata'])" | grep -q "^1$" \
  && ok "l'allarme compare in Centrale" || no "centrale lista" "-"
call A GET /api/patrol-dashboard | py "[s for s in d['slots'] if s['route_name']=='Notte $R'][0]['incident_id'] is not None" | grep -q True \
  && ok "cruscotto: slot mancato con collegamento all'allarme" || no "cruscotto" "-"

echo "patrol-missed: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
