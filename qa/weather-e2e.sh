#!/usr/bin/env bash
# Fase 4 «Meteo operativo» (mig 0168, modulo meteo): zone e previsioni scritte con una chiave di
# integrazione (come farà Youco), zona più vicina della sede, limiti meteo del tipo di attività,
# avvisi (mai blocchi) sull'intervento, ore asciutte dopo, interventi a rischio con il primo giorno
# buono, sostituzione della previsione, validazioni e permessi. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="wx$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Wx $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"meteo\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
K(){ curl -s -H "authorization: Bearer $1" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
KC(){ curl -s -o /dev/null -w "%{http_code}" -H "authorization: Bearer $1" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
KEY=$(call A POST /api/integration/keys '{"name":"Agente meteo","scopes":["plan"],"expires_days":30}' | py "d['key']")
KRO=$(call A POST /api/integration/keys '{"name":"Solo lettura","scopes":["orders.view_all"],"expires_days":30}' | py "d['key']")
# giorno D: il primo da lunedì a giovedì almeno 2 giorni dopo oggi (D+1 è lavorativo)
D=$(python -c "import datetime as t;d=t.date.today()+t.timedelta(days=2)
while d.weekday()>3: d+=t.timedelta(days=1)
print(d)")
D1=$(python -c "import datetime as t;print(t.date.fromisoformat('$D')+t.timedelta(days=1))")

echo "== zone scritte con la chiave =="
Z=$(K "$KEY" PUT /api/integration/weather/zones/MI-N '{"name":"Milano Nord","lat":45.52,"lon":9.19}')
echo "$Z" | py "d['created']" | grep -q True && ok "zona creata con la chiave" || no "zona" "$Z"
K "$KEY" PUT /api/integration/weather/zones/MI-N '{"name":"Milano Nord","lat":45.52,"lon":9.19}' | py "d['created']" | grep -q False && ok "stesso codice: aggiorna, nessun doppione" || no "upsert zona" "-"
ZB=$(K "$KEY" PUT /api/integration/weather/zones/MB '{"name":"Monza","lat":45.58,"lon":9.27}' | py "d['id']")
C=$(KC "$KRO" PUT /api/integration/weather/zones/X '{"name":"Xx","lat":45,"lon":9}'); [ "$C" = "403" ] && ok "chiave senza «Pianificare»: 403" || no "permesso" "$C"
C=$(KC "$KEY" PUT /api/integration/weather/zones/Y '{"name":"Yy","lat":95,"lon":9}'); [ "$C" = "400" ] && ok "posizione non valida: 400" || no "lat" "$C"

echo "== previsioni =="
SL="{\"zone\":\"MI-N\",\"source\":\"Agente Youco (prova)\",\"issued_at\":\"${D}T00:00:00Z\",\"slots\":[
 {\"from\":\"${D}T05:00:00Z\",\"to\":\"${D}T08:00:00Z\",\"rain_mm\":1,\"rain_prob\":40,\"wind_kmh\":10,\"temp_c\":8,\"condition\":\"nuvoloso\"},
 {\"from\":\"${D}T08:00:00Z\",\"to\":\"${D}T11:00:00Z\",\"rain_mm\":4,\"rain_prob\":80,\"wind_kmh\":15,\"temp_c\":10,\"condition\":\"pioggia\"},
 {\"from\":\"${D}T11:00:00Z\",\"to\":\"${D}T14:00:00Z\",\"rain_mm\":3,\"rain_prob\":70,\"wind_kmh\":20,\"gust_kmh\":45,\"temp_c\":11,\"condition\":\"pioggia\"},
 {\"from\":\"${D1}T05:00:00Z\",\"to\":\"${D1}T17:00:00Z\",\"rain_mm\":0,\"rain_prob\":5,\"wind_kmh\":8,\"temp_c\":12,\"condition\":\"sereno\"}]}"
P=$(K "$KEY" POST /api/integration/weather/slots "$SL")
echo "$P" | py "d['slots']" | grep -q "^4$" && ok "4 fasce di previsione scritte" || no "slots" "$P"
K "$KEY" POST /api/integration/weather/slots "{\"zone\":\"MB\",\"slots\":[{\"from\":\"${D}T05:00:00Z\",\"to\":\"${D}T17:00:00Z\",\"rain_mm\":0,\"condition\":\"sereno\"}]}" >/dev/null
C=$(KC "$KEY" POST /api/integration/weather/slots '{"zone":"NOPE","slots":[{"from":"2026-01-01T00:00:00Z","to":"2026-01-01T03:00:00Z"}]}'); [ "$C" = "400" ] && ok "zona inesistente: 400" || no "zona ignota" "$C"
C=$(KC "$KEY" POST /api/integration/weather/slots "{\"zone\":\"MI-N\",\"slots\":[{\"from\":\"${D}T05:00:00Z\",\"to\":\"${D}T05:00:00Z\"}]}"); [ "$C" = "400" ] && ok "fascia vuota: 400" || no "fascia" "$C"
C=$(KC "$KEY" POST /api/integration/weather/slots "{\"zone\":\"MI-N\",\"kind\":\"boh\",\"slots\":[{\"from\":\"${D}T05:00:00Z\",\"to\":\"${D}T06:00:00Z\"}]}"); [ "$C" = "400" ] && ok "tipo di dato non valido: 400" || no "kind" "$C"

echo "== sedi, tipo di attività, avvisi =="
S1=$(call A POST /api/sites '{"name":"Parco Nord","lat":45.515,"lon":9.20}' | py "d['id']")
S2=$(call A POST /api/sites '{"name":"Villa Monza","lat":45.59,"lon":9.28}' | py "d['id']")
AT=$(call A POST /api/activity-types '{"name":"Diserbo","base_minutes":120,"weather":{"rain_max_mm":2,"wind_max_kmh":40,"dry_hours_after":6}}' | py "d['id']")
call A GET /api/activity-types | py "[a['weather'] for a in d['items'] if a['id']=='$AT'][0]" | grep -q "'rain_max_mm': 2.0" && ok "limiti meteo salvati sul tipo di attività" || no "limiti" "-"
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
C=$(code A POST /api/activity-types '{"name":"Tmp","weather":{"temp_min_c":20,"temp_max_c":5}}'); [ "$C" = "400" ] && ok "temperature incoerenti rifiutate" || no "validazione limiti" "$C"
CK=$(call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}")
echo "$CK" | py "sorted(v['code'] for v in d['violations'] if v['code'].startswith('weather'))" | grep -q "'weather_dry_after', 'weather_rain'" && ok "zona più vicina: pioggia oltre il limite e niente ore asciutte dopo" || no "avviso" "$CK"
echo "$CK" | py "d['hard']" | grep -q False && ok "il meteo avvisa, non blocca" || no "hard" "$CK"
echo "$CK" | py "[v['message'] for v in d['violations'] if v['code']=='weather_rain'][0]" | grep -q "Milano Nord" && ok "il messaggio nomina la zona" || no "messaggio" "$CK"
CK2=$(call A POST /api/orders/check "{\"site_id\":\"$S2\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}")
echo "$CK2" | py "[v for v in d['violations'] if v['code'].startswith('weather')]" | grep -q "^\[\]$" && ok "sede di Monza: zona Monza asciutta, nessun avviso" || no "monza" "$CK2"
CKW=$(call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T11:00:00Z\",\"scheduled_end\":\"${D}T12:00:00Z\"}")
echo "$CKW" | py "'weather_wind' in [v['code'] for v in d['violations']]" | grep -q True && ok "raffiche oltre il vento massimo" || no "vento" "$CKW"
O=$(call A POST /api/orders "{\"title\":\"Diserbo viali\",\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}")
OID=$(echo "$O" | py "d.get('id','')"); [ -n "$OID" ] && ok "l'intervento si crea comunque (avviso)" || no "crea" "$O"

echo "== interventi a rischio, primo giorno buono =="
AR=$(call A GET /api/weather/at-risk?days=10)
echo "$AR" | py "[(x['id']=='$OID', x['suggested_start'][:16]) for x in d['items'] if x['id']=='$OID'][0]" | grep -q "(True, '${D1}T08:00')" && ok "a rischio, proposto il giorno dopo alla stessa ora" || no "at-risk" "$AR"
CKP=$(call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T09:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}")
echo "$CKP" | py "'weather_rain' in [v['code'] for v in d['violations']]" | grep -q False && ok "pioggia in proporzione: 1 ora di una fascia da 4 mm in 3 ore resta sotto 2 mm" || no "proporzione" "$CKP"
K "$KEY" PUT /api/integration/weather/zones/LODI '{"name":"Lodi","lat":45.31,"lon":9.50}' >/dev/null
K "$KEY" POST /api/integration/weather/slots "{\"zone\":\"LODI\",\"slots\":[{\"from\":\"${D}T05:00:00Z\",\"to\":\"${D}T17:00:00Z\",\"rain_mm\":20}]}" >/dev/null
S9=$(call A POST /api/sites '{"name":"Cascina Lodi","lat":45.32,"lon":9.49}' | py "d['id']")
O9=$(call A POST /api/orders "{\"title\":\"Diserbo cascina\",\"site_id\":\"$S9\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}" | py "d['id']")
call A GET /api/weather/at-risk?days=10 | py "[x['suggested_start'] for x in d['items'] if x['id']=='$O9']" | grep -q "^\[None\]$" && ok "oltre la previsione nessun giorno proposto" || no "copertura" "-"
FC=$(call A GET /api/weather/forecast?days=10)
echo "$FC" | py "(lambda z:(lambda x:(x['rain_mm'],x['rain_prob'],'Parco Nord' in [s['name'] for s in z['sites']]))([x for x in z['days'] if x['date']=='$D'][0]))([z for z in d['zones'] if z['code']=='MI-N'][0])"   | grep -q "^(8.0, 80, True)$" && ok "previsione del giorno: 8 mm, 80%, sede della zona" || no "forecast" "$FC"

echo "== sostituzione, sede fissata, eliminazione =="
K "$KEY" POST /api/integration/weather/slots "{\"zone\":\"MI-N\",\"slots\":[{\"from\":\"${D}T08:00:00Z\",\"to\":\"${D}T11:00:00Z\",\"rain_mm\":0.2},{\"from\":\"${D}T11:00:00Z\",\"to\":\"${D}T14:00:00Z\",\"rain_mm\":0}]}" >/dev/null
CK=$(call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}")
echo "$CK" | py "[v['code'] for v in d['violations'] if v['code'].startswith('weather')]" | grep -q "^\[\]$" && ok "nuova emissione sostituisce le fasce: avviso sparito" || no "sostituzione" "$CK"
K "$KEY" POST /api/integration/weather/slots "{\"zone\":\"MB\",\"slots\":[{\"from\":\"${D}T08:00:00Z\",\"to\":\"${D}T11:00:00Z\",\"rain_mm\":6}]}" >/dev/null
ZN=$(call A GET /api/weather/forecast | py "[z['id'] for z in d['zones'] if z['code']=='MI-N'][0]")
call A POST "/api/weather/zones/$ZB/sites" "{\"site_ids\":[\"$S1\"]}" | py "d['sites']" | grep -q "^1$" && ok "sede fissata su un'altra zona" || no "fissa" "-"
call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}" \
  | py "[v['message'] for v in d['violations'] if v['code']=='weather_rain']" | grep -q "Monza" && ok "la sede fissata usa la previsione della sua zona" || no "zona fissata" "-"
call A DELETE "/api/weather/zones/$ZB" | py "d['ok']" | grep -q True && ok "zona eliminata" || no "elimina" "-"
call A POST /api/orders/check "{\"site_id\":\"$S1\",\"activity_type_id\":\"$AT\",\"scheduled_start\":\"${D}T08:00:00Z\",\"scheduled_end\":\"${D}T10:00:00Z\"}" \
  | py "[v['code'] for v in d['violations'] if v['code'].startswith('weather')]" | grep -q "^\[\]$" && ok "dopo l'eliminazione la sede torna alla zona più vicina" || no "fallback" "-"
[ -n "$ZN" ] || no "zona MI-N" "-"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
