#!/usr/bin/env bash
# CA-1/CA-7 (ordini) + CA-2/CA-3 (punzonature di vigilanza), mig 0134.
#  Ordini:
#   - un lavoratore NON assegnato non avvia, non completa e non vede gli allegati di un ordine altrui
#   - l'assegnatario completa; un membro del gruppo assegnato avvia e completa
#   - con il modulo interventi spento nessuna transizione (anche per il pianificatore)
#  Punzonature:
#   - la guardia non riceve mai il codice atteso (elenco giri, avvio ronda, punzonatura)
#   - QR/NFC senza codice: generato dal sistema, non prevedibile; codice corto = avviso
#   - scansione -> metodo «qr»; digitato -> «qr_manuale» e anomalia a fine ronda
#   - giro «solo scansione»: il digitato è rifiutato
#   - un tentativo fallito non declassa una punzonatura riuscita; i tentativi restano contati
#   - il pianificatore non punzona la ronda di un altro
#   - distanza dal punto registrata; oltre il raggio = anomalia
# Tenant usa-e-getta, pacchetto completo.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="fi$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Fi $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"full\"}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"$2\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
U1=$(mk tec1 lavoratore); U2=$(mk tec2 lavoratore); U3=$(mk tec3 lavoratore); P=$(mk plan pianificatore)
login T1 "tec1@$SLUG.test" WorkPass9; login T2 "tec2@$SLUG.test" WorkPass9; login T3 "tec3@$SLUG.test" WorkPass9; login P "plan@$SLUG.test" WorkPass9
ver(){ q "SELECT version FROM app.work_orders WHERE id='$1'"; }

echo "== ordini: assegnatario =="
O1=$(call P POST /api/orders "{\"title\":\"Caldaia\",\"assigned_user\":\"$U1\"}" | py "d['id']")
C=$(code T2 POST "/api/orders/$O1/start" "{\"version\":$(ver $O1)}"); [ "$C" = "403" ] && ok "non assegnato non avvia (403)" || no "start altrui" "$C"
C=$(code T1 POST "/api/orders/$O1/start" "{\"version\":$(ver $O1)}"); [ "$C" = "200" ] && ok "assegnatario avvia" || no "start" "$C"
C=$(code T2 POST "/api/orders/$O1/complete" "{\"version\":$(ver $O1),\"outcome\":\"finto\"}"); [ "$C" = "403" ] && ok "non assegnato NON completa (403)" || no "CA-1 complete altrui" "$C"
[ "$(q "SELECT state FROM app.work_orders WHERE id='$O1'")" = "in_corso" ] && ok "ordine rimasto in corso" || no "stato" "-"
AT=$(curl -s -b "$J/T1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $(awk '$6=="yw_csrf"{print $7}' "$J/T1"|tail -1)" -H 'content-type: image/png' \
  -X POST "$BASE/api/orders/$O1/attachments?kind=photo&filename=f.png" --data-binary 'PNGDATA1234' | py "d['id']")
C=$(code T2 GET "/api/orders/attachments/$AT"); [ "$C" = "403" ] && ok "allegato non leggibile da chi non è sull'ordine" || no "allegato" "$C"
C=$(code T1 GET "/api/orders/attachments/$AT"); [ "$C" = "200" ] && ok "allegato leggibile dall'assegnatario" || no "allegato own" "$C"
C=$(code T1 POST "/api/orders/$O1/complete" "{\"version\":$(ver $O1),\"outcome\":\"ok\"}"); [ "$C" = "200" ] && ok "assegnatario completa" || no "complete" "$C"

echo "== ordini: gruppo =="
G=$(call A POST /api/user-groups '{"name":"Squadra nord"}' | py "d['id']")
call A POST "/api/user-groups/$G/members" "{\"user_id\":\"$U2\"}" >/dev/null
O2=$(call P POST /api/orders "{\"title\":\"Pompa\",\"assigned_group\":\"$G\"}" | py "d['id']")
C=$(code T3 POST "/api/orders/$O2/start" "{\"version\":$(ver $O2)}"); [ "$C" = "403" ] && ok "fuori dal gruppo non avvia" || no "start gruppo altrui" "$C"
C=$(code T2 POST "/api/orders/$O2/start" "{\"version\":$(ver $O2)}"); [ "$C" = "200" ] && ok "membro del gruppo avvia (CA-7)" || no "start gruppo" "$C"
C=$(code T2 POST "/api/orders/$O2/complete" "{\"version\":$(ver $O2),\"outcome\":\"ok\"}"); [ "$C" = "200" ] && ok "membro del gruppo completa" || no "complete gruppo" "$C"
O3=$(call P POST /api/orders "{\"title\":\"Off\",\"assigned_user\":\"$U1\"}" | py "d['id']")
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='interventi'"
C=$(code P POST "/api/orders/$O3/cancel" "{\"version\":$(ver $O3),\"reason\":\"x\"}"); [ "$C" = "403" ] && ok "modulo spento: nessuna transizione" || no "modulo" "$C"
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.tenant_modules SET state='attivo' WHERE tenant_id='$TID' AND module_code='interventi'"

echo "== punzonature: codice e configurazione =="
RT=$(call P POST /api/patrol-routes '{"name":"Notte"}' | py "d['id']")
CR=$(call P POST "/api/patrol-routes/$RT/checkpoints" '{"name":"Portineria","kind":"qr","lat":45.1,"lon":9.1,"radius_m":50}')
CP1=$(echo "$CR" | py "d['id']"); K1=$(echo "$CR" | py "d['code']")
echo "$CR" | py "d['generated'] and len(d['code'])>=19" | grep -q True && ok "QR senza codice: generato dal sistema ($K1)" || no "generato" "$CR"
CR2=$(call P POST "/api/patrol-routes/$RT/checkpoints" '{"name":"Magazzino","kind":"qr","code":"CP2"}')
CP2=$(echo "$CR2" | py "d['id']")
echo "$CR2" | py "bool(d.get('warning'))" | grep -q True && ok "codice corto: avviso al pianificatore" || no "avviso" "$CR2"
call T1 GET /api/patrol-routes | py "[c.get('code') for r in d for c in r['checkpoints']]" | grep -q "YW-\|CP2" && no "codice visibile alla guardia" "-" || ok "elenco giri: la guardia non riceve i codici"
call P GET /api/patrol-routes | grep -q "$K1" && ok "il pianificatore vede il codice" || no "codice planner" "-"
RO=$(call T1 POST "/api/patrol-routes/$RT/rounds" '{}')
RID=$(echo "$RO" | py "d['id']")
echo "$RO" | grep -q "$K1" && no "codice nell'avvio ronda" "-" || ok "avvio ronda: nessun codice"

echo "== punzonature: esecuzione =="
C=$(code P POST "/api/patrol-rounds/$RID/punch" "{\"checkpoint_id\":\"$CP1\",\"code\":\"$K1\",\"entry\":\"scansione\"}")
[ "$C" = "403" ] && ok "il pianificatore non punzona per un altro" || no "planner punch" "$C"
call T1 POST "/api/patrol-rounds/$RID/punch" "{\"checkpoint_id\":\"$CP1\",\"code\":\"$K1\",\"entry\":\"scansione\",\"lat\":45.1,\"lon\":9.1}" | py "d['ok']" | grep -q True \
  && ok "scansione corretta" || no "scan" "-"
[ "$(q "SELECT method||'/'||entry||'/'||distance_m FROM app.patrol_punches WHERE round_id='$RID' AND checkpoint_id='$CP1'")" = "qr/scansione/0" ] \
  && ok "metodo reale registrato (qr, scansione, 0 m)" || no "metodo" "$(q "SELECT method||'/'||entry FROM app.patrol_punches WHERE round_id='$RID'")"
call T1 POST "/api/patrol-rounds/$RID/punch" "{\"checkpoint_id\":\"$CP1\",\"code\":\"SBAGLIATO\",\"entry\":\"scansione\"}" >/dev/null
[ "$(q "SELECT ok::text||'/'||attempts||'/'||(last_fail_reason IS NOT NULL)::text FROM app.patrol_punches WHERE round_id='$RID' AND checkpoint_id='$CP1'")" = "true/2/true" ] \
  && ok "CA-3: tentativo fallito non declassa l'OK, conteggiato e motivato" || no "declassa" "$(q "SELECT ok::text||'/'||attempts FROM app.patrol_punches WHERE checkpoint_id='$CP1'")"
call T1 POST "/api/patrol-rounds/$RID/punch" "{\"checkpoint_id\":\"$CP2\",\"code\":\"CP2\"}" | py "d['ok'] and 'code' not in str(d['round'].get('checkpoints',[{}])[0].get('code') or '')" >/dev/null
[ "$(q "SELECT method||'/'||ok::text FROM app.patrol_punches WHERE round_id='$RID' AND checkpoint_id='$CP2'")" = "qr_manuale/true" ] \
  && ok "codice digitato: valido ma registrato come manuale" || no "manuale" "$(q "SELECT method FROM app.patrol_punches WHERE checkpoint_id='$CP2'")"
CL=$(call T1 POST "/api/patrol-rounds/$RID/close" '{}')
[ "$(q "SELECT count(*) FROM app.patrol_anomalies WHERE round_id='$RID' AND kind='inserimento_manuale'")" = "1" ] \
  && ok "a fine ronda: anomalia «codice digitato a mano»" || no "anomalia manuale" "$CL"

echo "== giro solo scansione + distanza =="
call P POST "/api/patrol-routes/$RT" '{"name":"Notte","manual_code_allowed":false}' >/dev/null
RID2=$(call T1 POST "/api/patrol-routes/$RT/rounds" '{}' | py "d['id']")
R2=$(call T1 POST "/api/patrol-rounds/$RID2/punch" "{\"checkpoint_id\":\"$CP2\",\"code\":\"CP2\",\"entry\":\"manuale\"}")
echo "$R2" | py "d['ok']==False and 'manuale' in d['reason']" | grep -q True && ok "giro solo scansione: digitato rifiutato" || no "solo scansione" "$R2"
call T1 POST "/api/patrol-rounds/$RID2/punch" "{\"checkpoint_id\":\"$CP1\",\"code\":\"$K1\",\"entry\":\"scansione\",\"lat\":45.103,\"lon\":9.1}" >/dev/null
call T1 POST "/api/patrol-rounds/$RID2/close" '{}' >/dev/null
[ "$(q "SELECT count(*) FROM app.patrol_anomalies WHERE round_id='$RID2' AND kind='posizione_distante'")" = "1" ] \
  && ok "scansione a ~330 m dal punto: anomalia «lontano dal punto»" || no "distanza" "$(q "SELECT distance_m FROM app.patrol_punches WHERE round_id='$RID2'")"
GV=$(call P GET "/api/patrol-rounds/$RID2" | py "d.get('user_email','')")
[ "$GV" = "tec1@$SLUG.test" ] && ok "dettaglio ronda: operatore per nome, non per codice" || no "operatore" "$GV"

echo "field-integrity: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
