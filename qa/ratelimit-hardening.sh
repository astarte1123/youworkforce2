#!/usr/bin/env bash
# Hardening: rate-limit sui tentativi di login (anti brute-force).
# Finestra fissa: 5 fallimenti / 5 min per chiave (slug|email|ip) e 20 / 5 min per account
# (acct|slug|email, indipendente dall'IP) -> 429 Retry-After.
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

# SA-4: cambiare X-Forwarded-For a ogni tentativo NON dà tentativi illimitati. Il limite per
# account (20 / 5 min, indipendente dall'IP) blocca comunque, anche per il superadmin.
ROT="rot-$(date +%s)-$RANDOM@acme.loneworker.eu"
first429=0
for i in $(seq 1 25); do
  c=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/login" -H 'content-type: application/json'     -H 'x-tenant-slug: acme' -H "x-forwarded-for: 10.$((RANDOM%250)).$((RANDOM%250)).$i"     -d "{\"email\":\"$ROT\",\"password\":\"wrong\"}")
  [ "$c" = "429" ] && { first429=$i; break; }
done
# Due configurazioni corrette (SA-4): senza proxy fidati (default) X-Forwarded-For è IGNORATO,
# l'IP resta quello del collegamento e il limite per IP (5) scatta al 6° tentativo; dietro un
# proxy fidato (YW_TRUSTED_PROXIES) l'IP cambia a ogni tentativo e ferma il limite per account
# (20) al 21°. In entrambi i casi la rotazione NON dà tentativi illimitati.
xff_ok(){ [ "$1" -ge 6 ] && [ "$1" -le 22 ]; }
xff_why(){ [ "$1" -le 6 ] && echo "intestazione ignorata, limite per IP" || echo "limite per account"; }
xff_ok "$first429" && ok "X-Forwarded-For a rotazione: bloccato al tentativo $first429 ($(xff_why "$first429"))" || no "XFF rotation" "primo 429 al tentativo $first429 (atteso tra 6 e 22)"
SROT="rot-$(date +%s)-$RANDOM@superadmin.test"
s429=0
for i in $(seq 1 25); do
  c=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json'     -H "x-forwarded-for: 172.16.$((RANDOM%250)).$i" -d "{\"email\":\"$SROT\",\"password\":\"wrong\"}")
  [ "$c" = "429" ] && { s429=$i; break; }
done
xff_ok "$s429" && ok "superadmin: XFF a rotazione bloccato al tentativo $s429 ($(xff_why "$s429"))" || no "super XFF rotation" "primo 429 al tentativo $s429"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
