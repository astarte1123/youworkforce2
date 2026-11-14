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

# teardown
sql "DELETE FROM app.users WHERE email IN ('$PLM') OR id='$VIC'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
