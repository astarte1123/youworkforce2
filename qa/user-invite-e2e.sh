#!/usr/bin/env bash
# TA-8 (decisione PO 26/11/2026): utenti invitati con link monouso, password scelta dalla persona
# (almeno 10 caratteri), nuovo link dall'amministratore, cambio password personale. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ui$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ui $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -w "%{http_code}" -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
acc(){ curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/invite/accept" -H 'content-type: application/json' -d "{\"slug\":\"$SLUG\",\"token\":\"$1\",\"password\":\"$2\"}"; }
tokof(){ python -c "import sys,urllib.parse as u;print(u.parse_qs(u.urlparse(sys.argv[1]).query)['token'][0])" "$1"; }
login A "a@$SLUG.test" DirectPass9 >/dev/null

R1=$(call A POST /api/users "{\"email\":\"neo@$SLUG.test\",\"role\":\"lavoratore\"}")
UID1=$(echo "$R1" | py "d['id']"); P1=$(echo "$R1" | py "d['invite']['invite_path']")
case "$P1" in /invite.html\?slug=$SLUG\&token=*) ok "utente creato senza password: link di accesso monouso";; *) no "invito" "$R1";; esac
T1=$(tokof "$P1")
C=$(login N "neo@$SLUG.test" "qualunque"); [ "$C" = "401" ] && ok "prima di usare l'invito non si accede" || no "accesso prima" "$C"
C=$(acc "$T1" "corta12"); [ "$C" = "400" ] && ok "password sotto i 10 caratteri rifiutata" || no "min 10" "$C"
C=$(acc "$T1" "PasswordNeo10"); [ "$C" = "200" ] && ok "la persona sceglie la sua password con il link" || no "accetta" "$C"
C=$(acc "$T1" "PasswordNeo11"); [ "$C" = "409" ] && ok "il link vale una volta sola" || no "riuso" "$C"
C=$(login N "neo@$SLUG.test" "PasswordNeo10"); [ "$C" = "200" ] && ok "accesso con la password scelta" || no "login" "$C"

P2=$(call A POST "/api/users/$UID1/invite" | py "d['invite_path']"); T2=$(tokof "$P2")
P3=$(call A POST "/api/users/$UID1/invite" | py "d['invite_path']"); T3=$(tokof "$P3")
C=$(acc "$T2" "PasswordNuova10"); [ "$C" = "410" ] || [ "$C" = "404" ] && ok "un nuovo link annulla il precedente ($C)" || no "vecchio link" "$C"
C=$(acc "$T3" "PasswordNuova10"); [ "$C" = "200" ] && ok "nuovo link di accesso (password dimenticata)" || no "nuovo link" "$C"
login N "neo@$SLUG.test" "PasswordNuova10" >/dev/null

C=$(code N POST /api/me/password '{"current":"sbagliata-sbagliata","new":"PasswordMia12"}'); [ "$C" = "403" ] && ok "cambio password con quella attuale errata: 403" || no "attuale errata" "$C"
C=$(code N POST /api/me/password '{"current":"PasswordNuova10","new":"corta"}'); [ "$C" = "400" ] && ok "nuova password troppo corta: 400" || no "corta" "$C"
C=$(code N POST /api/me/password '{"current":"PasswordNuova10","new":"PasswordMia12"}'); [ "$C" = "200" ] && ok "la persona cambia la propria password" || no "cambio" "$C"
C=$(login N2 "neo@$SLUG.test" "PasswordMia12"); [ "$C" = "200" ] && ok "accesso con la nuova password" || no "login nuova" "$C"
C=$(code A POST /api/users "{\"email\":\"x@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"abc\"}"); [ "$C" = "400" ] && ok "password iniziale via API sotto 8 caratteri rifiutata" || no "api min" "$C"
echo "user-invite: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
