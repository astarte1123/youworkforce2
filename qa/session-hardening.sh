#!/usr/bin/env bash
# Hardening: sessioni persistenti su DB (sopravvivono al riavvio del core) + logout
# server-side + scadenza. Prereqs: core su :8080, ywdb via `docker exec ywdb`.
# NB: la persistenza al RIAVVIO è intrinseca (la sessione è una riga in app.sessions);
# qui si prova il contratto DB-backed senza orchestrare un restart.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
dbq(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
JAR=$(mktemp)

# sessioni di QUESTO utente create da adesso (il totale scende quando il core cancella le scadute)
T0=$(dbq "SELECT now()")
B0=$(dbq "SELECT count(*) FROM app.sessions WHERE email='operatore@acme.loneworker.eu' AND created_at >= '$T0';")
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
B1=$(dbq "SELECT count(*) FROM app.sessions WHERE email='operatore@acme.loneworker.eu' AND created_at >= '$T0';")
[ "$B1" -gt "$B0" ] && ok "login crea una riga di sessione nel DB" || no "session persisted" "$B0->$B1"

ME=$(curl -s -b "$JAR" -H 'x-tenant-slug: acme' "$BASE/api/me")
echo "$ME" | grep -q 'operatore@acme' && ok "sessione valida (/me)" || no "me" "$ME"

# scadenza: forzo expires_at nel passato -> la sessione non deve più valere
# la sessione di QUESTO test (dal cookie), non «l'ultima creata»: altri test in parallelo ne creano
TOK=$(awk '$6=="yw_session"{print $7}' "$JAR" | tail -1)
dbq "UPDATE app.sessions SET expires_at = now() - interval '1 minute' WHERE token='$TOK';" >/dev/null
EXP=$(curl -s -o /dev/null -w "%{http_code}" -b "$JAR" -H 'x-tenant-slug: acme' "$BASE/api/me")
[ "$EXP" = "401" ] && ok "sessione scaduta rifiutata (401)" || no "expiry" "$EXP"
# ripristino per testare il logout
dbq "UPDATE app.sessions SET expires_at = now() + interval '12 hours' WHERE token='$TOK';" >/dev/null

LO=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/api/logout")
echo "$LO" | grep -q '"ok":true' && ok "logout ok" || no "logout" "$LO"
GONE=$(dbq "SELECT count(*) FROM app.sessions WHERE token='$TOK';")
[ "$GONE" = "0" ] && ok "logout elimina la riga di sessione (server-side)" || no "logout delete" "$GONE"
AFT=$(curl -s -o /dev/null -w "%{http_code}" -b "$JAR" -H 'x-tenant-slug: acme' "$BASE/api/me")
[ "$AFT" = "401" ] && ok "dopo logout /me -> 401" || no "post-logout" "$AFT"

# cookie manomesso -> nessuna sessione
BAD=$(curl -s -o /dev/null -w "%{http_code}" -H 'cookie: yw_session=deadbeef' -H 'x-tenant-slug: acme' "$BASE/api/me")
[ "$BAD" = "401" ] && ok "cookie di sessione non valido -> 401" || no "bad cookie" "$BAD"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
