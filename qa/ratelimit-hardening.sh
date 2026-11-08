#!/usr/bin/env bash
# Hardening: rate-limit sui tentativi di login (anti brute-force).
# Finestra fissa: RL_MAX=5 fallimenti / 5 min per chiave (slug|email|ip) -> 429 Retry-After.
# Prereqs: core su :8080.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }

# email inesistente e unica per run (non blocca utenti reali, non eredita stato)
ATK="atk-$(date +%s)@acme.loneworker.eu"
login(){ curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d "{\"email\":\"$1\",\"password\":\"$2\"}"; }

allbad=1
for i in 1 2 3 4 5; do
  c=$(login "$ATK" "wrong")
  [ "$c" = "401" ] || allbad=0
done
[ "$allbad" = "1" ] && ok "primi 5 tentativi errati -> 401" || no "pre-block attempts" "atteso 401"

C6=$(login "$ATK" "wrong")
[ "$C6" = "429" ] && ok "6° tentativo bloccato -> 429 (Retry-After)" || no "block" "$C6"

# anche la password giusta resta bloccata finché dura il blocco (chiave già bloccata)
CB=$(login "$ATK" "demo")
[ "$CB" = "429" ] && ok "chiave bloccata: anche credenziali valide -> 429" || no "blocked valid" "$CB"

# un utente diverso (chiave diversa) NON è impattato
CM=$(login "mario@acme.loneworker.eu" "demo")
[ "$CM" = "200" ] && ok "utente legittimo (chiave diversa) non impattato -> 200" || no "legit" "$CM"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
