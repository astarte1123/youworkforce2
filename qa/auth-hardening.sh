#!/usr/bin/env bash
# Hardening password: argon2id con verifica retro-compatibile e re-hash opportunistico.
# Prereqs: core su :8080, ywdb raggiungibile via `docker exec ywdb`.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
dbq(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }

EMAIL="auditor@acme.loneworker.eu"   # ruolo sola lettura, demo
# forza un hash legacy (sha256:salt) per provare la retro-compatibilità + re-hash
SALT="00112233445566778899aabbccddeeff"
LEG="$SALT:$(python -c "import hashlib,sys;print(hashlib.sha256((sys.argv[1]+'demo').encode()).hexdigest())" "$SALT")"
dbq "UPDATE app.users SET pass_hash='$LEG' WHERE email='$EMAIL';" >/dev/null
BEFORE=$(dbq "SELECT left(pass_hash,8) FROM app.users WHERE email='$EMAIL';")
[ "$BEFORE" = "00112233" ] && ok "hash legacy impostato" || no "setup legacy" "$BEFORE"

# login con hash legacy deve funzionare
C=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}")
[ "$C" = "200" ] && ok "login con hash legacy (retro-compatibile)" || no "legacy login" "$C"

# dopo il login l'hash deve essere migrato ad argon2id
sleep 1
AFTER=$(dbq "SELECT left(pass_hash,9) FROM app.users WHERE email='$EMAIL';")
[ "$AFTER" = '$argon2id' ] && ok "re-hash opportunistico ad argon2id" || no "rehash" "$AFTER"

# re-login con il nuovo hash argon2id
C2=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}")
[ "$C2" = "200" ] && ok "login con hash argon2id" || no "argon2 login" "$C2"

# password errata respinta
C3=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$EMAIL\",\"password\":\"sbagliata\"}")
[ "$C3" = "401" ] && ok "password errata respinta (401)" || no "bad pw" "$C3"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
