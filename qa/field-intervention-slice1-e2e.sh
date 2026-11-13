#!/usr/bin/env bash
# Slice 1 — prova di accettazione: tenant con SOLO interventi attivo (no workforce, no consuntivi).
# Il coordinatore assegna; il tecnico (PWA, via API) registra arrivo e avvio separati, checklist,
# note, ore, materiali, esito e invia il rapporto; il responsabile revisiona. Rapporto completo
# anche senza Consuntivi. Baseline: interventi spento -> ordini negati (403).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
# assetto: SOLO interventi (workforce+consuntivi spenti)
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','interventi','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code IN ('workforce','consuntivi')" >/dev/null
# tecnico (lavoratore) con password 'demo' (riusa hash di un utente esistente)
R=$RANDOM; HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
WEMAIL="tecnico-$R@globex.loneworker.eu"
WUID=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$WEMAIL','$HASH','lavoratore') RETURNING id")
# sede con indirizzo + coord + note d'accesso
SID=$(one "INSERT INTO app.sites(tenant_id,name,address,lat,lon,access_notes) VALUES('$TID','Cliente $R','Via Roma 1','45.46','9.19','Chiave sotto zerbino, cane in giardino') RETURNING id")

AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$ADMIN\",\"password\":\"demo\"}"
ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ" | tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

# coordinatore assegna un intervento SENZA attivare workforce
OID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/orders" -d "{\"title\":\"Riparazione caldaia\",\"site_id\":\"$SID\",\"instructions\":\"Valvola\",\"assigned_user\":\"$WUID\",\"checklist\":[\"Verifica pressione\",\"Sostituisci valvola\",\"Test tenuta\"]}" | jq "d.get('id','')")
[ -n "$OID" ] && ok "intervento assegnato (senza Workforce attivo)" || no "create ko" "-"

# tecnico: login PWA (stessa sessione/login)
WJ=$(mktemp); curl -s -o /dev/null -c "$WJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$WEMAIL\",\"password\":\"demo\"}"
WCS=$(awk '$6=="yw_csrf"{print $7}' "$WJ" | tail -1)
tec(){ curl -s -b "$WJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $WCS" "$@"; }
V=$(tec "$BASE/api/orders" | jq "[o['version'] for o in d if o['id']=='$OID'][0]")
# il tecnico vede le note d'accesso del proprio ordine
tec "$BASE/api/orders" | jq "[o.get('site_access') for o in d if o['id']=='$OID'][0]" | grep -qi "zerbino" && ok "note d'accesso visibili al tecnico assegnato" || no "access ko" "-"
# arrivo + avvio separati
tec -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID/arrive" -d '{}' | grep -q 200 && ok "arrivo registrato" || no "arrive ko" "-"
tec -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID/start" -d "{\"version\":$V}" | grep -q 200 && ok "avvio registrato" || no "start ko" "-"
# esecuzione: checklist (done), materiali, ore, report, esito
tec -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID/report" -d '{"report":"Valvola sostituita, impianto testato","checklist":[{"label":"Verifica pressione","done":true},{"label":"Sostituisci valvola","done":true},{"label":"Test tenuta","done":true}],"materials":[{"descr":"Valvola 3/4","qta":1,"unita":"pz"}],"work_minutes":75,"outcome_kind":"completato","signed_by":"Mario Rossi"}' >/dev/null
AA=$(sql "SELECT (arrived_at IS NOT NULL)||'|'||(work_started_at IS NOT NULL)||'|'||work_minutes||'|'||outcome_kind||'|'||(jsonb_array_length(checklist))::text||'|'||(jsonb_array_length(materials))::text FROM app.work_orders WHERE id='$OID'")
echo "$AA" | grep -qE "^(t|true)\|(t|true)\|75\|completato\|3\|1$" && ok "esecuzione salvata (arrivo/avvio/ore/esito/checklist/materiali)" || no "exec ko" "$AA"
# completa
V2=$(tec "$BASE/api/orders" | jq "[o['version'] for o in d if o['id']=='$OID'][0]")
tec -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID/complete" -d "{\"version\":$V2,\"outcome\":\"OK\"}" | grep -q 200 && ok "intervento completato dal tecnico" || no "complete ko" "-"
# responsabile revisiona: rapporto completo anche SENZA consuntivi
RP=$(adm "$BASE/api/orders" | jq "[(o.get('report'),o.get('signed_by'),o.get('work_minutes')) for o in d if o['id']=='$OID'][0]")
echo "$RP" | grep -q "Mario Rossi" && echo "$RP" | grep -q "75" && ok "responsabile vede rapporto completo (senza Consuntivi)" || no "review ko" "$RP"

# baseline: interventi SPENTO -> ordini negati (403); SOS indipendente resta
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='interventi'" >/dev/null
C=$(adm -o /dev/null -w "%{http_code}" "$BASE/api/orders")
[ "$C" = 403 ] && ok "Campo spento -> ordini non utilizzabili (403)" || no "gating ko" "$C"
sql "UPDATE app.tenant_modules SET state='attivo' WHERE tenant_id='$TID' AND module_code='interventi'" >/dev/null

# teardown
sql "DELETE FROM app.work_order_attachments WHERE work_order_id='$OID'" >/dev/null
sql "DELETE FROM app.work_orders WHERE id='$OID'" >/dev/null
sql "DELETE FROM app.sites WHERE id='$SID'" >/dev/null
sql "DELETE FROM app.users WHERE id='$WUID'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
