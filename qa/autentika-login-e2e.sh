#!/usr/bin/env bash
# Login con Autentika (mig 0172), affiancato al login con password. Usa un finto Autentika locale
# (qa/oidc_mock.py, porta 18097, firme ES256 come quello vero). Verifica: configurazione dal
# superadmin, bottone solo se attivo, flusso code + PKCE completo, primo accesso (utente creato con il
# ruolo dal token), collegamento per email verificata, email non verificata non collegata, accesso
# negato dall'IdP, state monouso, nonce e firma sbagliati rifiutati, login con password ancora attivo,
# back-channel logout, logout verso Autentika, «crea al primo accesso» spento, login disattivato.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
M="http://127.0.0.1:18097"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
MOCK_PID=""
if ! curl -s -o /dev/null --max-time 2 "$M/.well-known/jwks.json"; then
  python -I qa/oidc_mock.py 18097 & MOCK_PID=$!
  for i in $(seq 1 30); do curl -s -o /dev/null --max-time 1 "$M/.well-known/jwks.json" && break; sleep 0.3; done
fi
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
SA(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
SAC(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="au$R"
TID=$(SA POST /api/superadmin/tenants "{\"slug\":\"$SLUG\",\"name\":\"Au $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\"]}" | py "d.get('id','')")
trap 'SA DELETE /api/superadmin/tenants/$TID "{\"slug\":\"$SLUG\"}" >/dev/null; [ -n "$MOCK_PID" ] && kill $MOCK_PID 2>/dev/null; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/a" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
AC=$(awk '$6=="yw_csrf"{print $7}' "$J/a" | tail -1)
curl -s -b "$J/a" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' -X POST "$BASE/api/users" \
  -d "{\"email\":\"mario@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"MarioPass9\"}" >/dev/null
MUSER(){ curl -s -o /dev/null -X POST "$M/_mock/user" -H 'content-type: application/json' -d "$1"; }
MODE(){ curl -s -o /dev/null -X POST "$M/_mock/mode" -H 'content-type: application/json' -d "{\"mode\":\"$1\"}"; }
loc(){ curl -s -o /dev/null -D - "$@" | tr -d '\r' | awk 'tolower($1)=="location:"{print $2}'; }
# un «browser»: avvio → Autentika → ritorno a YW; stampa la Location finale, i cookie nel jar $1
flow(){ local jar="$J/$1"; rm -f "$jar"
  local a; a=$(loc "$BASE/api/auth/oidc/start?tenant=$SLUG&return_to=/app.html")
  case "$a" in http://127.0.0.1:18097/*) ;; *) echo "START:$a"; return;; esac
  local b; b=$(loc "$a")
  case "$b" in "$BASE"/api/auth/oidc/callback*) ;; *) echo "AUTH:$b"; return;; esac
  echo "$b" > "$J/last_cb"
  loc -c "$jar" "$b"; }
ME(){ curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" "$BASE/api/me"; }
CFG(){ echo "{\"issuer\":\"$M\",\"client_id\":\"ywf-test\",\"client_secret\":\"s3cret\",\"redirect_uri\":\"$BASE/api/auth/oidc/callback\",\"resource\":\"ywf\",\"enabled\":$1,\"jit\":$2,\"role_map\":{\"admin\":\"tenant_admin\",\"pianificatore\":\"pianificatore\",\"member\":\"lavoratore\"}}"; }

echo "== configurazione dal superadmin =="
curl -s "$BASE/api/auth/providers?tenant=$SLUG" | py "d['autentika']" | grep -q False && ok "senza configurazione il bottone non compare" || no "providers vuoto" "-"
C=$(SAC POST /api/superadmin/tenants/$TID/autentika '{"issuer":"http://evil.example","client_id":"x","client_secret":"y","redirect_uri":"https://x/cb"}'); [ "$C" = "400" ] && ok "issuer http non locale rifiutato" || no "issuer" "$C"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/a" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/autentika" -d "$(CFG true true)")
[ "$C" = "401" ] || [ "$C" = "403" ] && ok "solo il superadmin configura Autentika" || no "permesso" "$C"
SA POST /api/superadmin/tenants/$TID/autentika "$(CFG true true)" | py "d['ok']" | grep -q True && ok "configurazione salvata (segreto cifrato)" || no "salva" "-"
SA GET /api/superadmin/tenants/$TID/autentika | py "(d['enabled'], d['has_secret'], 'client_secret' in d)" | grep -q "(True, True, False)" && ok "il segreto non torna mai indietro" || no "lettura" "-"
SA POST /api/superadmin/tenants/$TID/autentika/verify | py "d['ok']" | grep -q True && ok "verifica: discovery, chiavi, PKCE, credenziali del client" || no "verifica" "$(SA POST /api/superadmin/tenants/$TID/autentika/verify)"
curl -s "$BASE/api/auth/providers?tenant=$SLUG" | py "d['autentika']" | grep -q True && ok "attivo: la pagina di accesso mostra «Login con Autentika»" || no "providers" "-"

echo "== primo accesso =="
MODE ok; MUSER '{"sub":"user_aaaaaaaa-0000-0000-0000-000000000001","email":"giulia@example.test","email_verified":true,"name":"Giulia","roles":["member","pianificatore"]}'
A=$(loc "$BASE/api/auth/oidc/start?tenant=$SLUG&return_to=/app.html")
echo "$A" | grep -q "code_challenge_method=S256" && echo "$A" | grep -q "resource=ywf" && ok "avvio verso Autentika con PKCE S256 e risorsa" || no "avvio" "$A"
F=$(flow G); [ "$F" = "/app.html" ] && ok "ritorno all'app dopo l'accesso" || no "flusso" "$F"
ME G | py "(d['email'], d['role'])" | grep -q "('giulia@example.test', 'pianificatore')" && ok "utente creato al primo accesso con il ruolo da Autentika (pianificatore)" || no "jit" "$(ME G)"
F=$(loc -c "$J/G2" "$(cat "$J/last_cb")"); echo "$F" | grep -q "auth_error=" && ok "lo stesso ritorno non si usa due volte" || no "state riusato" "$F"
F=$(flow G3); ME G3 | py "d['email']" | grep -q giulia && ok "secondo accesso: stesso utente (per sub)" || no "secondo accesso" "$F"

echo "== utente esistente =="
MUSER "{\"sub\":\"user_bbbbbbbb-0000-0000-0000-000000000002\",\"email\":\"mario@$SLUG.test\",\"email_verified\":false,\"roles\":[\"member\"]}"
F=$(flow X); echo "$F" | grep -q "auth_error=" && ok "email non verificata: nessun collegamento all'utente esistente" || no "non verificata" "$F"
MUSER "{\"sub\":\"user_bbbbbbbb-0000-0000-0000-000000000003\",\"email\":\"MARIO@$SLUG.test\",\"email_verified\":true,\"roles\":[\"admin\"]}"
F=$(flow MA); ME MA | py "(d['email'], d['role'])" | grep -q "('mario@$SLUG.test', 'lavoratore')" && ok "email verificata: collegato all'utente esistente, ruolo YW invariato" || no "collegamento" "$(ME MA)"
C=$(curl -s -o /dev/null -w "%{http_code}" -c "$J/MP" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"mario@$SLUG.test\",\"password\":\"MarioPass9\"}")
[ "$C" = "200" ] && ok "lo stesso utente entra ancora con la password" || no "password" "$C"

echo "== errori =="
MODE deny; F=$(loc "$(loc "$BASE/api/auth/oidc/start?tenant=$SLUG&return_to=/app.html")" | xargs -I{} curl -s -o /dev/null -D - "{}" | tr -d '\r' | awk 'tolower($1)=="location:"{print $2}')
echo "$F" | grep -q "auth_error=accesso%20negato" && ok "Autentika nega l'accesso: messaggio chiaro" || no "deny" "$F"
MODE bad_nonce; F=$(flow N); echo "$F" | grep -q "auth_error=" && ok "nonce diverso: rifiutato" || no "nonce" "$F"
MODE bad_sig; F=$(flow S); echo "$F" | grep -q "auth_error=firma" && ok "firma con un'altra chiave: rifiutata" || no "firma" "$F"
MODE ok

echo "== logout =="
SID=$(python -c "import urllib.parse as u,sys;print('x')")
LT=$(curl -s "$M/_mock/logout_token?sid=__none__" | py "d['logout_token']")
C=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/auth/backchannel-logout" --data-urlencode "logout_token=$LT"); [ "$C" = "200" ] && ok "back-channel logout accettato" || no "bcl" "$C"
# sid della sessione di G3: dal database
GSID=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT auth_sid FROM app.sessions WHERE tenant_id='$TID' AND auth_sid IS NOT NULL ORDER BY expires_at DESC LIMIT 1" | tr -d '[:space:]')
NS=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT count(*) FROM app.sessions WHERE tenant_id='$TID' AND auth_sid='$GSID'" | tr -d '[:space:]')
LT=$(curl -s "$M/_mock/logout_token?sid=$GSID" | py "d['logout_token']")
curl -s -X POST "$BASE/api/auth/backchannel-logout" --data-urlencode "logout_token=$LT" | py "d['sessions']" | grep -q "^$NS$" && ok "logout in Autentika: chiusa la sessione YW di quel sid" || no "bcl sessioni" "$NS"
LT2=$(curl -s "$M/_mock/logout_token?sid=$GSID&aud=altro" | py "d['logout_token']")
C=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/auth/backchannel-logout" --data-urlencode "logout_token=$LT2"); [ "$C" = "400" ] && ok "logout_token per un altro client: rifiutato" || no "bcl aud" "$C"
F=$(flow L); LC=$(awk '$6=="yw_csrf"{print $7}' "$J/L" | tail -1)
curl -s -o /dev/null -b "$J/L" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $LC" -X POST "$BASE/api/logout"
for i in 1 2 3 4 5; do curl -s "$M/_mock/log" | py "len(d)>0 and d[-1]['id_token_hint']" | grep -q True && break; sleep 0.5; done
curl -s "$M/_mock/log" | py "len(d)>0 and d[-1]['id_token_hint'] and d[-1]['client_id']=='ywf-test'" | grep -q True && ok "logout da YW: chiusa anche la sessione di Autentika (id_token_hint)" || no "rp logout" "$(curl -s $M/_mock/log)"

echo "== impostazioni =="
SA POST /api/superadmin/tenants/$TID/autentika "$(CFG true false)" >/dev/null
MUSER '{"sub":"user_cccccccc-0000-0000-0000-000000000004","email":"sconosciuto@example.test","email_verified":true,"roles":["member"]}'
F=$(flow U); echo "$F" | grep -q "auth_error=utente%20non%20presente" && ok "senza «crea al primo accesso»: utente sconosciuto non entra" || no "jit off" "$F"
SA POST /api/superadmin/tenants/$TID/autentika "$(CFG false true)" >/dev/null
F=$(loc "$BASE/api/auth/oidc/start?tenant=$SLUG&return_to=/app.html"); echo "$F" | grep -q "auth_error=login%20con%20Autentika%20non%20attivo" && ok "disattivato: il login con Autentika non parte" || no "disattivo" "$F"
curl -s "$BASE/api/auth/providers?tenant=$SLUG" | py "d['autentika']" | grep -q False && ok "disattivato: il bottone sparisce" || no "bottone off" "-"
F=$(loc "$BASE/api/auth/oidc/start?tenant=$SLUG&return_to=//evil.example"); echo "$F" | grep -q "^/?auth_error" && ok "ritorno verso un altro sito ignorato" || no "open redirect" "$F"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
