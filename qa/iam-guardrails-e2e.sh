#!/usr/bin/env bash
# TA-1 / TA-2 / TA-3 — regole di sicurezza della gestione utenti e permessi.
#  TA-1 "non si concede ciò che non si possiede": un delegato con users.manage (senza admin) non si
#       promuove, non crea amministratori, non crea/assegna ruoli con permessi che non ha, non tocca
#       la matrice dei ruoli oltre i propri permessi, non modifica né elimina un amministratore.
#       Può invece gestire utenti con permessi inferiori ai suoi.
#  TA-2 la sessione rilegge ruolo ed esistenza dell'utente a ogni richiesta (eliminato -> 401,
#       retrocesso -> nuovo ruolo subito).
#  TA-3 il tenant non resta mai senza amministratore (modifica di sé o di altri, eliminazione).
# Tenant usa-e-getta, eliminato a fine suite.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
J=$(mktemp -d)
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print(d$1)" 2>/dev/null; }
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="iam$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"IAM $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" | jget "['id']")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }

login(){ curl -s -o /dev/null -w "%{http_code}" -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
# call <jar> <method> <path> [body] -> body; callc -> http code
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
callc(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }

login A "a@$SLUG.test" DirectPass9 >/dev/null
AID=$(call A GET /api/me | jget "['tenant_id']" >/dev/null; call A GET /api/users | python -c "import sys,json;print([u['id'] for u in json.load(sys.stdin) if u['email']=='a@$SLUG.test'][0])")
DID=$(call A POST /api/users "{\"email\":\"d@$SLUG.test\",\"role\":\"operatore\",\"password\":\"DelegPass9\"}" | jget "['id']")
RID=$(call A POST /api/custom-roles '{"name":"Delegato utenti","capabilities":["users.manage"]}' | jget "['id']")
call A POST "/api/custom-roles/$RID/users" "{\"user_id\":\"$DID\"}" >/dev/null
RADM=$(call A POST /api/custom-roles '{"name":"Super nascosto","capabilities":["admin"]}' | jget "['id']")
[ -n "$DID" ] && [ -n "$RID" ] && [ -n "$RADM" ] && ok "setup: delegato con users.manage (senza admin)" || no "setup" "D=$DID R=$RID RADM=$RADM"
login D "d@$SLUG.test" DelegPass9 >/dev/null

echo "== TA-1 non si concede ciò che non si possiede =="
C=$(callc D POST "/api/users/$DID" '{"role":"tenant_admin"}');     [ "$C" = "403" ] && ok "delegato non si promuove ad amministratore" || no "self-promote" "$C"
C=$(callc D POST /api/users "{\"email\":\"x@$SLUG.test\",\"role\":\"tenant_admin\",\"password\":\"Xpass1234\"}"); [ "$C" = "403" ] && ok "delegato non crea amministratori" || no "create admin" "$C"
C=$(callc D POST /api/custom-roles '{"name":"Furbo","capabilities":["admin"]}'); [ "$C" = "403" ] && ok "delegato non crea ruoli con admin" || no "role admin" "$C"
C=$(callc D POST /api/custom-roles '{"name":"Furbo2","capabilities":["gdpr.manage"]}'); [ "$C" = "403" ] && ok "delegato non crea ruoli con permessi che non ha" || no "role gdpr" "$C"
C=$(callc D POST "/api/custom-roles/$RADM/users" "{\"user_id\":\"$DID\"}"); [ "$C" = "403" ] && ok "delegato non si assegna un ruolo con admin" || no "assign admin role" "$C"
C=$(callc D POST /api/capabilities '{"role":"operatore","capability":"admin","granted":true}'); [ "$C" = "403" ] && ok "delegato non dà admin a un ruolo" || no "matrix admin" "$C"
C=$(callc D DELETE /api/capabilities); [ "$C" = "403" ] && ok "ripristino matrice riservato all'amministratore" || no "reset" "$C"
C=$(callc D POST "/api/users/$AID" '{"password":"Hijack1234"}'); [ "$C" = "403" ] && ok "delegato non cambia la password dell'amministratore" || no "admin pw" "$C"
C=$(callc D DELETE "/api/users/$AID");                          [ "$C" = "403" ] && ok "delegato non elimina l'amministratore" || no "del admin" "$C"
LID=$(call D POST /api/users "{\"email\":\"l@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"LavPass99\"}" | jget "['id']")
[ -n "$LID" ] && ok "delegato crea un lavoratore (permessi inferiori)" || no "create worker" "-"
C=$(callc D POST "/api/users/$LID" '{"phone":"+393331112222"}');  [ "$C" = "200" ] && ok "delegato modifica il lavoratore" || no "edit worker" "$C"

echo "== TA-4 permessi, non nome del ruolo =="
C=$(callc D GET /api/setup-status); [ "$C" = "403" ] && ok "configurazione guidata senza settings.manage -> 403" || no "setup 403" "$C"
E=$(call D GET /api/modules/loneworker/tools | python -c "import sys,json;d=json.load(sys.stdin);print(any(t.get('editable') for t in d) if isinstance(d,list) else 'vietato')" 2>/dev/null || echo vietato)
case "$E" in False|vietato) ok "strumenti di modulo non modificabili senza settings.manage ($E)";; *) no "tools ro" "$E";; esac
RS=$(call A POST /api/custom-roles '{"name":"Impostazioni","capabilities":["settings.manage"]}' | jget "['id']")
call A POST "/api/custom-roles/$RS/users" "{\"user_id\":\"$DID\"}" >/dev/null
C=$(callc D GET /api/setup-status); [ "$C" = "200" ] && ok "con settings.manage delegato -> 200 (senza essere admin)" || no "setup 200" "$C"
E=$(call D GET /api/modules/loneworker/tools | python -c "import sys,json;d=json.load(sys.stdin);d=d if isinstance(d,list) else d.get('tools',[]);print(any(t.get('editable') for t in d))")
[ "$E" = "True" ] && ok "strumenti di modulo modificabili con settings.manage" || no "tools rw" "$E"
call A DELETE "/api/custom-roles/$RS/users/$DID" >/dev/null

echo "== TA-2 sessioni sempre aggiornate =="
login L "l@$SLUG.test" LavPass99 >/dev/null
call A POST "/api/users/$LID" '{"role":"reperibile"}' >/dev/null
RL=$(call L GET /api/me | jget "['role']"); [ "$RL" = "reperibile" ] && ok "ruolo cambiato: la sessione aperta lo vede subito" || no "role live" "$RL"
call A DELETE "/api/users/$LID" >/dev/null
C=$(callc L GET /api/me); [ "$C" = "401" ] && ok "utente eliminato: sessione non più valida (401)" || no "deleted session" "$C"

echo "== TA-3 mai senza amministratore =="
C=$(callc A POST "/api/users/$AID" '{"role":"operatore"}'); [ "$C" = "409" ] && ok "unico amministratore non si retrocede (409)" || no "self demote" "$C"
BID=$(call A POST /api/users "{\"email\":\"b@$SLUG.test\",\"role\":\"tenant_admin\",\"password\":\"AdminB999\"}" | jget "['id']")
[ -n "$BID" ] && ok "amministratore crea un altro amministratore" || no "create admin B" "-"
login B "b@$SLUG.test" AdminB999 >/dev/null
C=$(callc B DELETE "/api/users/$AID"); [ "$C" = "200" ] && ok "con due amministratori se ne può eliminare uno" || no "del A" "$C"
C=$(callc B POST "/api/users/$BID" '{"role":"operatore"}'); [ "$C" = "409" ] && ok "ora B è l'unico: non si retrocede (409)" || no "B demote" "$C"

echo "iam-guardrails: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
