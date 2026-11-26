#!/usr/bin/env bash
# Fase 2 «Integrazioni» (mig 0166): chiavi di integrazione, inserimento/aggiornamento per codice
# esterno (idempotente), vincoli sugli interventi dall'esterno, elenco delle modifiche, import CSV.
#  - la chiave si mostra una volta; agisce con i permessi scelti tra quelli di chi la crea;
#    non gestisce altre chiavi; revocata o di un'altra azienda non vale; senza cookie niente CSRF
#  - PUT per codice: la seconda chiamata aggiorna, non duplica; riferimenti per codice; codice
#    sconosciuto -> 400; intervento chiuso non si modifica
#  - changes: interventi e consuntivi cambiati da `since`, `until` come prossimo since
#  - import CSV: `;` o `,`, righe errate nel rapporto senza fermare le altre
# Tenant usa-e-getta.
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
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="in$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"In $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"cantieri\",\"workforce\",\"consuntivi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
# chiamate dell'ERP: solo la chiave, nessun cookie, nessun token CSRF
K(){ curl -s -H "authorization: Bearer $KEY" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
KC(){ curl -s -o /dev/null -w "%{http_code}" -H "authorization: Bearer $KEY" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
login A "a@$SLUG.test" DirectPass9
PL=$(call A POST /api/users "{\"email\":\"p@$SLUG.test\",\"role\":\"pianificatore\",\"password\":\"PlanPass99\"}" | py "d['id']")
login P "p@$SLUG.test" PlanPass99
G=$(call A POST /api/user-groups '{"name":"Squadra Nord","kind":"squadra"}' | py "d['id']")

echo "== chiavi =="
C=$(code P POST /api/integration/keys '{"name":"ERP","scopes":["plan"]}'); [ "$C" = "403" ] && ok "senza «impostazioni» non si creano chiavi" || no "permesso chiavi" "$C"
C=$(code A POST /api/integration/keys '{"name":"ERP","scopes":["admin"]}'); [ "$C" = "400" ] && ok "«admin» non si dà a una chiave" || no "scope admin" "$C"
KJ=$(call A POST /api/integration/keys '{"name":"Gestionale ERP","scopes":["plan","orders.view_all","timesheets.view_all"],"expires_days":365}')
KEY=$(echo "$KJ" | py "d['key']"); KID=$(echo "$KJ" | py "d['id']")
echo "$KEY" | grep -q "^ywk_" && ok "chiave creata (mostrata una volta)" || no "chiave" "$KJ"
KL=$(call A GET /api/integration/keys)
echo "$KL" | py "any(k in d['keys'][0] for k in ('key','key_hash','hash'))" | grep -q False && ok "l'elenco non contiene la chiave né l'impronta" || no "elenco" "$KL"
[ "$(q "SELECT count(*) FROM app.api_keys WHERE key_hash='$KEY'")" = "0" ] && ok "in DB solo l'impronta" || no "hash" "-"
C=$(KC GET /api/integration/keys); [ "$C" = "403" ] && ok "una chiave non gestisce le chiavi" || no "chiave su chiavi" "$C"
C=$(curl -s -o /dev/null -w "%{http_code}" -H "authorization: Bearer ywk_xxxx_yyyy" "$BASE/api/orders"); [ "$C" = "401" ] && ok "chiave inventata: 401" || no "chiave falsa" "$C"
C=$(KC POST /api/users '{"email":"x@x.test","role":"lavoratore","password":"WorkPass99"}'); [ "$C" = "403" ] && ok "permessi limitati a quelli scelti (creare persone: 403)" || no "scope limitato" "$C"

echo "== inserimento/aggiornamento per codice =="
CJ=$(K PUT /api/integration/clients/CLI-001 '{"name":"Condominio Aurora","vat":"01234567890"}')
echo "$CJ" | py "d['created']" | grep -q True && ok "cliente creato per codice" || no "cliente" "$CJ"
K PUT /api/integration/clients/CLI-001 '{"name":"Condominio Aurora srl"}' | py "d['created']" | grep -q False && ok "stessa chiamata: aggiornato, non duplicato" || no "idempotenza" "-"
[ "$(q "SELECT count(*)||'/'||max(name)||'/'||max(vat) FROM app.clients WHERE tenant_id='$TID' AND external_ref='CLI-001'")" = "1/CondominioAurorasrl/01234567890" ] && ok "campi non passati restano" || no "merge" "$(q "SELECT name,vat FROM app.clients WHERE tenant_id='$TID'")"
C=$(KC PUT /api/integration/sites/S-1 '{"name":"Aurora"}'); [ "$C" = "403" ] && ok "sedi senza il permesso «sedi»: 403" || no "sede scope" "$C"
SJ=$(call A PUT /api/integration/sites/S-1 '{"name":"Aurora giardino","address":"Via Pacini 22, Milano","access_windows":[{"days":"1111100","from":"08:00","to":"17:00"}]}')
SITE=$(echo "$SJ" | py "d.get('id','')")
[ -n "$SITE" ] && ok "sede per codice (dalla console)" || no "sede" "$SJ"
CM=$(K PUT /api/integration/commesse/COM-77 '{"name":"Riqualificazione parco","client_ref":"CLI-001","site_ref":"S-1","start_date":"2027-03-01","budget_amount":12000}')
echo "$CM" | py "d['created']" | grep -q True && ok "commessa con cliente e sede per codice" || no "commessa" "$CM"
C=$(KC PUT /api/integration/commesse/COM-78 '{"name":"X","client_ref":"NON-ESISTE"}'); [ "$C" = "400" ] && ok "codice sconosciuto: 400" || no "ref ignoto" "$C"
OJ=$(K PUT /api/integration/orders/ODL-1 "{\"title\":\"Taglio erba\",\"site_ref\":\"S-1\",\"client_ref\":\"CLI-001\",\"team\":\"Squadra Nord\",\"scheduled_start\":\"2027-03-01T09:00:00+01:00\",\"scheduled_end\":\"2027-03-01T11:00:00+01:00\"}")
O1=$(echo "$OJ" | py "d['id']")
[ "$(q "SELECT state||'/'||(assigned_group='$G')::text FROM app.work_orders WHERE id='$O1'")" = "assegnato/true" ] && ok "intervento assegnato alla squadra per nome" || no "intervento" "$OJ"
C=$(KC PUT /api/integration/orders/ODL-2 '{"title":"Sabato","site_ref":"S-1","scheduled_start":"2027-03-06T09:00:00+01:00","scheduled_end":"2027-03-06T10:00:00+01:00"}')
[ "$C" = "409" ] && ok "vincoli anche dall'esterno: fuori orario della sede 409" || no "vincolo esterno" "$C"
K PUT /api/integration/orders/ODL-1 '{"title":"Taglio erba e rifilature","priority":3}' >/dev/null
\
[ "$(q "SELECT priority FROM app.work_orders WHERE id='$O1'")" = "3" ] && ok "intervento aggiornato per codice" || no "aggiorna intervento" "-"

echo "== elenco delle modifiche =="
T0=$(python -c "import datetime as d;print((d.datetime.now(d.timezone.utc)-d.timedelta(minutes=10)).isoformat())")
CH=$(K GET "/api/integration/changes?since=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$T0")")
echo "$CH" | py "any(o['ref']=='ODL-1' and o['site_ref']=='S-1' and o['team']=='Squadra Nord' for o in d['orders'])" | grep -q True && ok "modifiche: l'intervento con i suoi codici" || no "changes" "$CH"
UNTIL=$(echo "$CH" | py "d['until']")
CH2=$(K GET "/api/integration/changes?since=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$UNTIL")")
echo "$CH2" | py "len(d['orders'])" | grep -q "^0$" && ok "dal nuovo «since»: niente già consegnato" || no "cursore" "$CH2"
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.work_orders SET state='completato', work_minutes=95, updated_at=now() WHERE id='$O1'"
K GET "/api/integration/changes?since=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$UNTIL")" | py "[o['state']+'/'+str(o['work_minutes']) for o in d['orders']]" | grep -q "completato/95" \
  && ok "chiusura e minuti lavorati tornano all'ERP" || no "chiusura" "-"
C=$(KC PUT /api/integration/orders/ODL-1 '{"title":"Riapro"}'); [ "$C" = "409" ] && ok "intervento completato non si modifica" || no "chiuso" "$C"

echo "== import CSV =="
CSV=$'ref;name;address;vat\nCLI-002;Villa Rosa;Via Roma 1, Monza;\nCLI-001;Condominio Aurora srl;;\n;Senza codice;;\nCLI-003;;;'
IJ=$(curl -s -H "authorization: Bearer $KEY" -H 'content-type: text/csv' -X POST "$BASE/api/integration/import/clients" --data-binary "$CSV")
echo "$IJ" | py "(d['created'],d['updated'],len(d['errors']))" | grep -q "(1, 1, 2)" && ok "import: 1 nuovo, 1 aggiornato, 2 righe errate nel rapporto" || no "import" "$IJ"
echo "$IJ" | py "sorted(e['row'] for e in d['errors'])" | grep -q "\[4, 5\]" && ok "righe errate indicate per numero" || no "righe" "$IJ"
C=$(curl -s -o /dev/null -w "%{http_code}" -H "authorization: Bearer $KEY" -H 'content-type: text/csv' -X POST "$BASE/api/integration/import/utenti" --data-binary "$CSV")
[ "$C" = "404" ] && ok "entità non importabile: 404" || no "entità" "$C"

echo "== revoca e isolamento =="
call A DELETE "/api/integration/keys/$KID" >/dev/null
C=$(KC GET /api/orders); [ "$C" = "401" ] && ok "chiave revocata: 401" || no "revoca" "$C"
[ "$(q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND action LIKE 'integration_%' AND actor LIKE '%chiave%'")" != "0" ] && ok "registro attività: azioni della chiave riconoscibili" || no "audit" "-"

echo "integrations: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
