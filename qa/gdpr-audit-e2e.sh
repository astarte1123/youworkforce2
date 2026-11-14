#!/usr/bin/env bash
# Fase 4 Tappa 4 — GDPR UI backend + audit viewer. Prova: audit-log list (admin 200 / non-admin 403,
# filtro per azione), retention info, gdpr export (profilo), anonymize (blocca self, esegue, scrubba PII
# e blocca login), e che ogni operazione lasci traccia nell'audit log.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
R=$RANDOM; HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
# utente cavia + un pianificatore (non-admin) per i 403
VICM="vic-$R@globex.loneworker.eu"
VIC=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) VALUES('$TID','$VICM','$HASH','lavoratore','+390000000') RETURNING id")
PLM="pl-ga-$R@globex.loneworker.eu"
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$PLM','$HASH','pianificatore')" >/dev/null
login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
PJ=$(login "$PLM");   PCS=$(awk '$6=="yw_csrf"{print $7}' "$PJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
pl(){  curl -s -b "$PJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $PCS" "$@"; }
code(){ "$@" -o /dev/null -w '%{http_code}'; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }

# 1) audit-log: admin 200 con rows+actions
A=$(adm "$BASE/api/audit-log?limit=50"); echo "$A" | jget "len(d['rows'])>=0 and isinstance(d['actions'],list)" | grep -q True && ok "audit-log admin -> 200 (rows+actions)" || no "audit admin" "$A"
# 2) non-admin -> 403
[ "$(code pl "$BASE/api/audit-log")" = 403 ] && ok "audit-log pianificatore -> 403" || no "audit 403" "-"
# 3) retention info admin 200
RET=$(adm "$BASE/api/gdpr/retention"); echo "$RET" | jget "'enabled' in d and 'days' in d" | grep -q True && ok "gdpr/retention admin -> 200" || no "retention" "$RET"
# 4) retention non-admin 403
[ "$(code pl "$BASE/api/gdpr/retention")" = 403 ] && ok "gdpr/retention pianificatore -> 403" || no "retention 403" "-"
# 5) export: profilo con email+phone
EXP=$(adm "$BASE/api/gdpr/export/$VIC"); echo "$EXP" | jget "d['profile']['email']=='$VICM'" | grep -q True && ok "gdpr export -> profilo utente" || no "export" "$EXP"
# 6) export non-admin -> 403
[ "$(code pl "$BASE/api/gdpr/export/$VIC")" = 403 ] && ok "gdpr export pianificatore -> 403" || no "export 403" "-"
# 7) anonymize self -> 400
[ "$(code adm -X POST "$BASE/api/gdpr/anonymize/$(one "SELECT id FROM app.users WHERE email='$ADMIN'")")" = 400 ] && ok "anonymize self -> 400" || no "anon self" "-"
# 8) anonymize cavia -> 200
adm -o /dev/null -w '%{http_code}' -X POST "$BASE/api/gdpr/anonymize/$VIC" | grep -q 200 && ok "anonymize cavia -> 200" || no "anon vic" "-"
# 9) PII scrubbed: phone NULL, email anon, anonymized_at set
PH=$(one "SELECT coalesce(phone,'NULL') FROM app.users WHERE id='$VIC'"); AN=$(one "SELECT (anonymized_at IS NOT NULL) FROM app.users WHERE id='$VIC'")
[ "$PH" = NULL ] && [ "$AN" = t ] && ok "PII rimossa (phone NULL, anonymized_at set)" || no "scrub" "phone=$PH anon=$AN"
# 10) login cavia ora fallisce (pass bloccata)
LC=$(mktemp); CODE=$(curl -s -o /dev/null -w '%{http_code}' -c "$LC" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$VICM\",\"password\":\"demo\"}")
[ "$CODE" != 200 ] && ok "login utente anonimizzato fallisce ($CODE)" || no "login still ok" "-"
# 11) audit traccia: filtro action=gdpr_anonymize ha >=1 voce
AF=$(adm "$BASE/api/audit-log?action=gdpr_anonymize&limit=10"); echo "$AF" | jget "len(d['rows'])>=1 and all(r['action']=='gdpr_anonymize' for r in d['rows'])" | grep -q True && ok "audit filtro action=gdpr_anonymize" || no "audit filter" "$AF"
# 12) audit filtro actor (admin email) ha voci
AA=$(adm "$BASE/api/audit-log?actor=admin@globex&limit=10"); echo "$AA" | jget "len(d['rows'])>=1" | grep -q True && ok "audit filtro actor" || no "audit actor" "$AA"

# 13) paginazione: total presente; limit=1 -> 1 riga; offset cambia riga
P1=$(adm "$BASE/api/audit-log?limit=1&offset=0"); P2=$(adm "$BASE/api/audit-log?limit=1&offset=1")
T=$(echo "$P1" | jget "d['total']"); N1=$(echo "$P1" | jget "len(d['rows'])")
ID1=$(echo "$P1" | jget "d['rows'][0]['id'] if d['rows'] else ''"); ID2=$(echo "$P2" | jget "d['rows'][0]['id'] if d['rows'] else ''")
[ "$N1" = 1 ] && [ "$T" -ge 2 ] 2>/dev/null && [ "$ID1" != "$ID2" ] && ok "paginazione: total=$T, limit=1, offset cambia riga" || no "pagination" "n1=$N1 total=$T id1=$ID1 id2=$ID2"
# 14) ricerca testo libero q -> trova gdpr_anonymize (match su azione)
QF=$(adm "$BASE/api/audit-log?q=anonymize&limit=50"); echo "$QF" | jget "len(d['rows'])>=1 and all('anonymize' in (r['action']+r['actor']+str(r['detail'])) for r in d['rows'])" | grep -q True && ok "ricerca testo libero q=anonymize" || no "q search" "$QF"
# 15) filtro date: since nel futuro -> 0 righe
FUT=$(adm "$BASE/api/audit-log?since=2999-01-01T00:00:00Z"); echo "$FUT" | jget "d['total']==0 and len(d['rows'])==0" | grep -q True && ok "filtro since futuro -> 0 righe" || no "since filter" "$FUT"
# 16) export CSV: content-type csv + header colonne + >=1 riga dati; non-admin 403
CT=$(curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -o /tmp/au.csv -w '%{content_type}' "$BASE/api/audit-log/export?limit=10")
head -1 /tmp/au.csv | grep -q "quando,attore,azione,dettaglio" && echo "$CT" | grep -q "text/csv" && [ "$(wc -l < /tmp/au.csv)" -ge 2 ] && ok "export CSV (header + righe + content-type)" || no "csv export" "ct=$CT"
[ "$(code pl "$BASE/api/audit-log/export")" = 403 ] && ok "export CSV pianificatore -> 403" || no "csv 403" "-"

# 17) gdpr/users: admin 200 con rows+total+roles; non-admin 403
GU=$(adm "$BASE/api/gdpr/users?limit=10"); echo "$GU" | jget "isinstance(d['rows'],list) and 'total' in d and isinstance(d['roles'],list) and d['total']>=1" | grep -q True && ok "gdpr/users admin -> 200 (rows+total+roles)" || no "gdpr users" "$GU"
[ "$(code pl "$BASE/api/gdpr/users")" = 403 ] && ok "gdpr/users pianificatore -> 403" || no "gusers 403" "-"
# 18) filtro status=anonymized -> solo anonimizzati (cavia inclusa)
GA=$(adm "$BASE/api/gdpr/users?status=anonymized&limit=50"); echo "$GA" | jget "len(d['rows'])>=1 and all(r['anonymized_at'] is not None for r in d['rows'])" | grep -q True && ok "gdpr/users filtro status=anonymized" || no "gusers status" "$GA"
# 19) filtro status=active -> nessuno anonimizzato
GAC=$(adm "$BASE/api/gdpr/users?status=active&limit=50"); echo "$GAC" | jget "all(r['anonymized_at'] is None for r in d['rows'])" | grep -q True && ok "gdpr/users filtro status=active" || no "gusers active" "$GAC"
# 20) filtro role=pianificatore -> trova il planner creato
GR=$(adm "$BASE/api/gdpr/users?role=pianificatore&limit=50"); echo "$GR" | jget "len(d['rows'])>=1 and all(r['role']=='pianificatore' for r in d['rows'])" | grep -q True && ok "gdpr/users filtro role" || no "gusers role" "$GR"
# 21) paginazione: limit=1 -> 1 riga, total>=2
GP=$(adm "$BASE/api/gdpr/users?limit=1&offset=0"); GPT=$(echo "$GP" | jget "d['total']"); GPN=$(echo "$GP" | jget "len(d['rows'])")
[ "$GPN" = 1 ] && [ "$GPT" -ge 2 ] 2>/dev/null && ok "gdpr/users paginazione (limit=1, total=$GPT)" || no "gusers pagination" "n=$GPN total=$GPT"
# 22) export CSV: header colonne + content-type + >=1 riga; non-admin 403
GCT=$(curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -o /tmp/gu.csv -w '%{content_type}' "$BASE/api/gdpr/users/export")
head -1 /tmp/gu.csv | grep -q "email,ruolo,telefono,creato,stato,anonimizzato_il" && echo "$GCT" | grep -q "text/csv" && [ "$(wc -l < /tmp/gu.csv)" -ge 2 ] && ok "gdpr/users export CSV (header+righe+content-type)" || no "gusers csv" "ct=$GCT"
[ "$(code pl "$BASE/api/gdpr/users/export")" = 403 ] && ok "gdpr/users export pianificatore -> 403" || no "gusers csv 403" "-"
# 23) export CSV tracciato in audit (gdpr_users_export)
GEA=$(adm "$BASE/api/audit-log?action=gdpr_users_export&limit=5"); echo "$GEA" | jget "len(d['rows'])>=1" | grep -q True && ok "audit traccia gdpr_users_export" || no "gusers audit" "$GEA"

# teardown
sql "DELETE FROM app.users WHERE email IN ('$PLM') OR id='$VIC'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
