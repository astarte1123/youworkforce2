#!/usr/bin/env bash
# Fase 1 «Lavoro sul campo a scala reale» (mig 0165): tipi di attività, finestre orarie delle sedi,
# mezzi (stato, prenotazioni senza sovrapposizioni, scadenze), qualifiche richieste da attività e
# mezzi, controllo dei vincoli su creazione/modifica/assegnazione dell'intervento, scadenziario,
# piani di manutenzione con tipo di attività. Tenant usa-e-getta.
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
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="fc$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Fc $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"workforce\",\"qualifiche\",\"mezzi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' ${FORCE:+-H "x-force: 1"} -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' ${FORCE:+-H "x-force: 1"} -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A PUT /api/settings/timezone '{"timezone":"Europe/Rome"}' >/dev/null
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
W1=$(mk w1); W2=$(mk w2); W3=$(mk w3)
QP=$(call A POST /api/qualifications '{"name":"Abilitazione PLE"}' | py "d['id']")
call A POST "/api/users/$W1/qualifications" "{\"qualification_id\":\"$QP\",\"expires_at\":\"2030-01-01T00:00:00Z\"}" >/dev/null
call A POST "/api/users/$W2/qualifications" "{\"qualification_id\":\"$QP\",\"expires_at\":\"2027-01-01T00:00:00Z\"}" >/dev/null   # scaduta prima del 2027-03
G1=$(call A POST /api/user-groups '{"name":"Squadra Potature","kind":"squadra"}' | py "d['id']")
G2=$(call A POST /api/user-groups '{"name":"Squadra Sud","kind":"squadra"}' | py "d['id']")
for u in $W1 $W2; do call A POST "/api/user-groups/$G1/members" "{\"user_id\":\"$u\"}" >/dev/null; done
for u in $W2 $W3; do call A POST "/api/user-groups/$G2/members" "{\"user_id\":\"$u\"}" >/dev/null; done

echo "== sede con finestra oraria =="
C=$(code A POST /api/sites '{"name":"X","access_windows":[{"days":"0000000","from":"08:00","to":"17:00"}]}'); [ "$C" = "400" ] && ok "finestra senza giorni rifiutata" || no "finestra 400" "$C"
SITE=$(call A POST /api/sites '{"name":"Parco Nord","timezone":"Europe/Rome","access_windows":[{"days":"1111100","from":"08:00","to":"17:00"}]}' | py "d['id']")
call A GET /api/sites | py "[s for s in d if s['id']=='$SITE'][0]['access_windows'][0]['to']" | grep -q "17:00" && ok "finestra salvata e restituita" || no "finestra" "-"

echo "== tipi di attività =="
C=$(code A POST /api/activity-types '{"name":"Xx","unit":"ettari"}'); [ "$C" = "400" ] && ok "unità non valida: 400" || no "unità" "$C"
AT=$(call A POST /api/activity-types "{\"name\":\"Potatura in quota\",\"unit\":\"n\",\"base_minutes\":60,\"minutes_per_unit\":30,\"crew_size\":2,\"qualification_ids\":[\"$QP\"],\"assets\":[{\"kind\":\"Piattaforma\",\"qty\":1}]}" | py "d['id']")
[ -n "$AT" ] && ok "tipo di attività creato" || no "tipo" "-"
C=$(code A POST /api/activity-types '{"name":"Potatura in quota"}'); [ "$C" = "409" ] && ok "nome doppio: 409" || no "doppio" "$C"
call A GET /api/activity-types | py "[x for x in d['items'] if x['id']=='$AT'][0]" | grep -q "Abilitazione PLE" && ok "elenco con qualifica richiesta" || no "elenco" "-"
call A GET /api/activity-types | py "[x for x in d['items'] if x['id']=='$AT'][0]['assets'][0]['kind']" | grep -q "^piattaforma$" && ok "mezzo richiesto normalizzato (piattaforma ×1)" || no "mezzo" "-"

echo "== mezzi =="
P1=$(call A POST /api/assets "{\"name\":\"Piattaforma 18 m\",\"kind\":\"piattaforma\",\"required_qualification_id\":\"$QP\"}" | py "d['id']")
P2=$(call A POST /api/assets '{"name":"Piattaforma 24 m","kind":"piattaforma","status":"fermo"}' | py "d['id']")
C=$(code A POST /api/assets '{"name":"Z","status":"rotto"}'); [ "$C" = "400" ] && ok "stato del mezzo non valido: 400" || no "stato" "$C"
call A GET /api/assets | py "[a for a in d if a['id']=='$P2'][0]['status']" | grep -q fermo && ok "stato del mezzo salvato" || no "stato salvato" "-"

MON="2027-03-01"   # lunedì
echo "== controllo dei vincoli (anteprima) =="
CK=$(call A POST /api/orders/check "{\"site_id\":\"$SITE\",\"activity_type_id\":\"$AT\",\"quantity\":2,\"scheduled_start\":\"${MON}T09:00:00+01:00\",\"assigned_group\":\"$G2\"}")
echo "$CK" | py "d['estimated_minutes']" | grep -q "^120$" && ok "durata stimata: 60 + 30×2 = 120 min" || no "stima" "$CK"
echo "$CK" | py "d['scheduled_end'][:16]" | grep -q "2027-03-01T10:00" && ok "fine calcolata dalla stima" || no "fine" "$CK"
echo "$CK" | py "any(v['code']=='qualification' and v['hard'] for v in d['violations'])" | grep -q True && ok "squadra senza PLE valida: vincolo duro" || no "qualifica" "$CK"
echo "$CK" | py "any(v['code']=='asset_missing' and not v['hard'] for v in d['violations'])" | grep -q True && ok "piattaforma non prenotata: avviso" || no "mezzo mancante" "$CK"

echo "== creazione e assegnazione =="
C=$(code A POST /api/orders "{\"title\":\"Potatura viale\",\"site_id\":\"$SITE\",\"activity_type_id\":\"$AT\",\"quantity\":2,\"scheduled_start\":\"${MON}T09:00:00+01:00\",\"assigned_group\":\"$G2\"}")
[ "$C" = "409" ] && ok "intervento con squadra senza qualifica: 409" || no "409 qualifica" "$C"
C=$(FORCE=1 code A POST /api/orders "{\"title\":\"Forzato\",\"site_id\":\"$SITE\",\"scheduled_start\":\"${MON}T09:00:00+01:00\",\"activity_type_id\":\"$AT\",\"assigned_group\":\"$G2\"}")
[ "$C" = "200" ] && ok "«Forza» crea comunque" || no "forza" "$C"
O1J=$(call A POST /api/orders "{\"title\":\"Potatura viale\",\"site_id\":\"$SITE\",\"activity_type_id\":\"$AT\",\"quantity\":2,\"scheduled_start\":\"${MON}T09:00:00+01:00\",\"assigned_group\":\"$G1\",\"asset_ids\":[\"$P1\"]}")
O1=$(echo "$O1J" | py "d.get('id','')")
[ -n "$O1" ] && ok "squadra con PLE e piattaforma prenotata: creato" || no "creazione" "$O1J"
echo "$O1J" | py "[v['code'] for v in d['warnings']]" | grep -q "crew" && no "squadra di 2: nessun avviso persone atteso" "$O1J" || ok "2 persone disponibili: nessun avviso"
[ "$(q "SELECT to_char(scheduled_end AT TIME ZONE 'Europe/Rome','HH24:MI') FROM app.work_orders WHERE id='$O1'")" = "11:00" ] && ok "fine salvata dalla stima" || no "fine salvata" "-"
[ "$(q "SELECT count(*) FROM app.asset_bookings WHERE work_order_id='$O1'")" = "1" ] && ok "piattaforma prenotata sull'intervento" || no "prenotazione" "-"
C=$(code A POST /api/orders "{\"title\":\"Doppio\",\"site_id\":\"$SITE\",\"scheduled_start\":\"${MON}T10:00:00+01:00\",\"scheduled_end\":\"${MON}T12:00:00+01:00\",\"assigned_group\":\"$G1\",\"asset_ids\":[\"$P1\"]}")
[ "$C" = "409" ] && ok "stessa piattaforma nello stesso orario: 409" || no "doppia prenotazione" "$C"
C=$(code A POST /api/orders "{\"title\":\"Fermo\",\"site_id\":\"$SITE\",\"scheduled_start\":\"${MON}T13:00:00+01:00\",\"scheduled_end\":\"${MON}T14:00:00+01:00\",\"assigned_group\":\"$G1\",\"asset_ids\":[\"$P2\"]}")
[ "$C" = "409" ] && ok "mezzo fermo: 409" || no "mezzo fermo" "$C"
C=$(code A POST /api/orders "{\"title\":\"Sabato\",\"site_id\":\"$SITE\",\"scheduled_start\":\"2027-03-06T09:00:00+01:00\",\"scheduled_end\":\"2027-03-06T10:00:00+01:00\",\"assigned_group\":\"$G1\"}")
[ "$C" = "409" ] && ok "sabato fuori dall'orario della sede: 409" || no "finestra" "$C"
C=$(code A POST /api/orders "{\"title\":\"Sera\",\"site_id\":\"$SITE\",\"scheduled_start\":\"${MON}T16:00:00+01:00\",\"scheduled_end\":\"${MON}T18:00:00+01:00\"}")
[ "$C" = "409" ] && ok "finisce dopo le 17: 409" || no "finestra sera" "$C"
V=$(q "SELECT version FROM app.work_orders WHERE id='$O1'")
C=$(code A POST "/api/orders/$O1/edit" "{\"version\":$V,\"scheduled_start\":\"2027-03-02T09:00:00+01:00\"}")
[ "$C" = "200" ] && ok "spostato a martedì" || no "spostamento" "$C"
[ "$(q "SELECT to_char(starts_at AT TIME ZONE 'Europe/Rome','DD HH24:MI') FROM app.asset_bookings WHERE work_order_id='$O1'")" = "0209:00" ] && ok "la prenotazione segue l'intervento" || no "prenotazione spostata" "$(q "SELECT starts_at FROM app.asset_bookings WHERE work_order_id='$O1'")"
V=$(q "SELECT version FROM app.work_orders WHERE id='$O1'")
C=$(code A POST "/api/orders/$O1/assign-group" "{\"version\":$V,\"group_id\":\"$G2\"}")
[ "$C" = "409" ] && ok "riassegnare a una squadra senza PLE: 409" || no "assegnazione" "$C"
docker exec -i ywdb psql -U postgres -d youworkforce -qc "INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at,kind) VALUES('$TID','$W1','2027-03-03T00:00:00+01:00','2027-03-04T00:00:00+01:00','ferie')" 2>/dev/null \
 || docker exec -i ywdb psql -U postgres -d youworkforce -qc "INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$W1','2027-03-03T00:00:00+01:00','2027-03-04T00:00:00+01:00')"
C=$(code A POST /api/orders "{\"title\":\"Ferie\",\"site_id\":\"$SITE\",\"scheduled_start\":\"2027-03-03T09:00:00+01:00\",\"scheduled_end\":\"2027-03-03T10:00:00+01:00\",\"assigned_user\":\"$W1\"}")
[ "$C" = "409" ] && ok "persona assente: 409" || no "assenza" "$C"
CH=$(call A GET "/api/orders/checks?from=2027-03-01T00:00:00Z&to=2027-03-08T00:00:00Z")
echo "$CH" | py "len(d)>=1" | grep -q True && ok "vincoli della settimana per l'agenda" || no "checks" "$CH"

echo "== scadenziario =="
call A POST "/api/assets/$P1/deadlines" '{"kind":"revisione","due_date":"2020-01-01"}' >/dev/null
DL=$(call A GET '/api/deadlines?days=60')
echo "$DL" | py "any(i['kind']=='mezzo' and i['days_left']<0 for i in d['items'])" | grep -q True && ok "revisione del mezzo scaduta nello scadenziario" || no "scadenza mezzo" "$DL"
echo "$DL" | py "any(i['kind']=='qualifica' and i['user_id']=='$W2' for i in d['items'])" | grep -q True && ok "patentino scaduto nello scadenziario" || no "scadenza qualifica" "-"
CK=$(call A POST /api/orders/check "{\"site_id\":\"$SITE\",\"scheduled_start\":\"2027-03-04T09:00:00+01:00\",\"scheduled_end\":\"2027-03-04T10:00:00+01:00\",\"assigned_group\":\"$G1\",\"asset_ids\":[\"$P1\"]}")
echo "$CK" | py "any(v['code']=='asset_deadline' for v in d['violations'])" | grep -q True && ok "mezzo con revisione scaduta: avviso" || no "avviso scadenza" "$CK"
BK=$(call A GET "/api/asset-bookings?from=2027-03-01T00:00:00Z&to=2027-03-08T00:00:00Z")
echo "$BK" | py "any(b['order_title']=='Potatura viale' and b['group_name']=='Squadra Potature' for b in d)" | grep -q True && ok "prenotazioni con intervento e squadra" || no "prenotazioni" "$BK"

echo "== piani di manutenzione con tipo di attività =="
PL=$(call A POST /api/maintenance-plans "{\"title\":\"Potature annuali\",\"site_id\":\"$SITE\",\"every_days\":365,\"next_date\":\"2027-03-05\",\"activity_type_id\":\"$AT\",\"quantity\":4}" | py "d['id']")
call A POST "/api/maintenance-plans/$PL/generate" '{}' >/dev/null
[ "$(q "SELECT estimated_minutes||'/'||(EXTRACT(EPOCH FROM scheduled_end-scheduled_start)/60)::int FROM app.work_orders WHERE tenant_id='$TID' AND activity_type_id='$AT' AND title LIKE 'Potature annuali%'")" = "180/180" ] \
  && ok "intervento generato con tipo, quantità e durata (60 + 30×4)" || no "piano" "$(q "SELECT estimated_minutes FROM app.work_orders WHERE tenant_id='$TID' AND title LIKE 'Potature annuali%'")"
C=$(code A DELETE "/api/activity-types/$AT"); [ "$C" = "409" ] && ok "tipo in uso: non si elimina (409)" || no "elimina in uso" "$C"

echo "== modulo mezzi spento =="
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='mezzi'"
C=$(code A POST /api/orders "{\"title\":\"Senza modulo\",\"scheduled_start\":\"${MON}T09:00:00+01:00\",\"scheduled_end\":\"${MON}T10:00:00+01:00\",\"asset_ids\":[\"$P1\"]}")
[ "$C" = "403" ] && ok "prenotare mezzi senza il modulo: 403" || no "modulo" "$C"

echo "field-constraints: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
