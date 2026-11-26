#!/usr/bin/env bash
# Fase 4b: previsioni da MET Norway (mig 0169). Usa un finto api.met.no locale (qa/metno_mock.py,
# porta 18098): il core di test va avviato con YW_METNO_URL=http://127.0.0.1:18098.
# Verifica: scelta della fonte per tenant, scaricamento per zona con User-Agent dell'applicazione,
# fasce orarie poi da 6 ore senza sovrapposizioni, conversione m/s → km/h, cache di almeno 3 ore,
# richiesta condizionale (304), errore registrato sulla zona, avvisi sull'intervento, permessi.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sqlq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
MLOG="$J/mock.log"; : > "$MLOG"
MLOG_W=$(cygpath -m "$MLOG" 2>/dev/null || echo "$MLOG")
MOCK_PID=""
if ! curl -s -o /dev/null --max-time 2 "http://127.0.0.1:18098/x"; then
  python -I qa/metno_mock.py "$MLOG_W" 18098 & MOCK_PID=$!
  for i in $(seq 1 20); do curl -s -o /dev/null --max-time 1 "http://127.0.0.1:18098/x" && break; sleep 0.3; done
else echo "(finto MET già in ascolto: le richieste non finiranno nel registro di questo test)"; fi
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="mn$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Mn $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"meteo\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; [ -n "$MOCK_PID" ] && kill $MOCK_PID 2>/dev/null; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A POST /api/users '{"email":"pl@'$SLUG'.test","password":"DirectPass9","role":"pianificatore"}' >/dev/null
login P "pl@$SLUG.test" DirectPass9

echo "== fonte delle previsioni =="
call A GET /api/weather/settings | py "d['provider']" | grep -q "^integrazione$" && ok "di serie: sistema collegato con la chiave" || no "default" "-"
C=$(code A POST /api/weather/refresh); [ "$C" = "400" ] && ok "con il sistema collegato non si scarica da qui" || no "refresh senza metno" "$C"
C=$(code P PUT /api/weather/settings '{"provider":"metno"}'); [ "$C" = "403" ] && ok "la fonte la sceglie chi gestisce le impostazioni" || no "permesso fonte" "$C"
C=$(code A PUT /api/weather/settings '{"provider":"openweather"}'); [ "$C" = "400" ] && ok "fonte sconosciuta: 400" || no "fonte" "$C"
call A PUT /api/weather/settings '{"provider":"metno"}' | py "d['provider']" | grep -q metno && ok "scelta MET Norway" || no "metno" "-"

echo "== scaricamento =="
ZID=$(call A PUT /api/integration/weather/zones/MI '{"name":"Milano","lat":45.464211,"lon":9.191383}' | py "d['id']")
RF=$(call P POST /api/weather/refresh)
echo "$RF" | py "(d['updated'], d['slots']>60)" | grep -q "(1, True)" && ok "zona scaricata: fasce orarie e da 6 ore" || no "scarica" "$RF"
grep -q '"lat": "45.4642", "lon": "9.1914"' "$MLOG" && ok "coordinate con 4 decimali" || no "coordinate" "$(head -c 300 "$MLOG")"
grep -q '"ua": "YouWorkforce/' "$MLOG" && ok "User-Agent che identifica l'applicazione" || no "user agent" "$(head -c 300 "$MLOG")"
OV=$(q "SELECT count(*) FROM app.weather_slots a JOIN app.weather_slots b ON a.zone_id=b.zone_id AND a.valid_from<b.valid_from AND a.valid_to>b.valid_from WHERE a.zone_id='$ZID'")
[ "$OV" = "0" ] && ok "nessuna fascia sovrapposta" || no "sovrapposizioni" "$OV"
q "SELECT count(*) FILTER (WHERE wind_kmh=18 AND gust_kmh=36) > 40 AND bool_and(source='MET Norway') FROM app.weather_slots WHERE zone_id='$ZID' AND valid_to-valid_from=interval '1 hour'" | grep -q "^t$" \
  && ok "vento da m/s a km/h (5 → 18, raffiche 10 → 36), fonte MET Norway" || no "conversione" "-"
q "SELECT expires_at >= now() + interval '170 minutes' FROM app.weather_zones WHERE id='$ZID'" | grep -q "^t$" && ok "cache di almeno 3 ore anche se MET dice 1 ora" || no "cache" "-"
N1=$(wc -l < "$MLOG")
call A POST /api/weather/refresh | py "d['updated']+d['not_modified']" | grep -q "^0$" && ok "zona aggiornata da poco: nessuna nuova richiesta" || no "rispetto cache" "-"
[ "$(wc -l < "$MLOG")" = "$N1" ] && ok "il finto MET non ha ricevuto richieste" || no "richieste extra" "-"
sqlq "UPDATE app.weather_zones SET expires_at=now()-interval '1 minute' WHERE id='$ZID'"
call A POST /api/weather/refresh | py "d['not_modified']" | grep -q "^1$" && ok "scaduta: richiesta condizionale, 304 niente di nuovo" || no "304" "-"
tail -1 "$MLOG" | grep -q '"ims": "Wed, 25 Nov 2026 10:00:00 GMT"' && ok "If-Modified-Since con l'ultima data ricevuta" || no "ims" "$(tail -1 "$MLOG")"

echo "== avvisi e stato in console =="
D1=$(python -c "import datetime as t;print((t.datetime.now(t.timezone.utc)+t.timedelta(days=1)).date())")
S1=$(call A POST /api/sites '{"name":"Parco Sempione","lat":45.472,"lon":9.177}' | py "d['id']")
AT=$(call A POST /api/activity-types '{"name":"Taglio erba","base_minutes":120,"weather":{"rain_max_mm":2}}' | py "d['id']")
CK=$(call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D1}T08:00:00Z\",\"scheduled_end\":\"${D1}T10:00:00Z\"}")
echo "$CK" | py "[v['message'] for v in d['violations'] if v['code']=='weather_rain'][0]" | grep -q "previsti 4 mm" && ok "previsione MET → avviso pioggia sull'intervento" || no "avviso" "$CK"
call A GET /api/weather/forecast | py "[z['source'] for z in d['zones']][0]" | grep -q "MET Norway" && ok "la previsione dichiara la fonte (citazione CC BY)" || no "fonte" "-"
call A PUT /api/integration/weather/zones/ERR '{"name":"Zona errore","lat":1.0,"lon":9.0}' >/dev/null
call A POST /api/weather/refresh | py "d['failed']" | grep -q "^1$" && ok "errore di MET contato" || no "errore" "-"
call A GET /api/weather/settings | py "[z['error'] for z in d['zones'] if z['code']=='ERR'][0]" | grep -q "errore 500" && ok "errore spiegato sulla zona" || no "errore zona" "-"
q "SELECT expires_at BETWEEN now()+interval '25 minutes' AND now()+interval '35 minutes' FROM app.weather_zones WHERE tenant_id='$TID' AND code='ERR'" | grep -q "^t$" && ok "nuovo tentativo dopo 30 minuti" || no "backoff" "-"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
