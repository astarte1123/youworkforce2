#!/usr/bin/env bash
# "Cosa fare adesso": GET /api/home-tasks ritorna azioni in sospeso tagliate sul RUOLO, dai dati
# reali. Ogni ruolo tenant può chiamarlo. Tenant di prova: acme.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }
login(){ local u=$1 j=$2; curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$u@acme.loneworker.eu\",\"password\":\"demo\"}"; }

JO=$(mktemp); login operatore "$JO"
R=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/home-tasks")
echo "$R" | grep -q '"items"' && ok "operatore: home-tasks risponde con items" || { no "nessun items" "$R"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }
# struttura: ogni item ha title/link/tone
echo "$R" | jv 'all(all(k in it for k in ["title","link","tone"]) for it in d["items"])' | grep -qi true \
  && ok "ogni azione ha title/link/tone" || no "campi azione mancanti" "$R"
# operatore: se ci sono allarmi aperti, deve vedere l'azione verso la centrale (tone bad)
OPEN=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "select count(*) from app.incidents i join app.tenants t on t.id=i.tenant_id where t.slug='acme' and i.state='aperto'")
if [ "${OPEN:-0}" -gt 0 ]; then
  echo "$R" | jv 'any(it["link"]=="centrale" and it["tone"]=="bad" for it in d["items"])' | grep -qi true \
    && ok "operatore con allarmi aperti → azione verso Centrale (tone bad)" || no "azione allarmi mancante" "$R"
else ok "nessun allarme aperto: nessuna azione attesa (ok)"; fi

# pianificatore: azioni verso ordini/turni
JP=$(mktemp); login pianificatore "$JP"
RP=$(curl -s -b "$JP" -H 'x-tenant-slug: acme' "$BASE/api/home-tasks")
echo "$RP" | jv 'all(it["link"] in ("ordini","turni") for it in d["items"]) if d["items"] else True' | grep -qi true \
  && ok "pianificatore: azioni verso Interventi/Turni" || no "azioni pianificatore errate" "$RP"

# ruolo diverso = contenuto diverso (operatore ha azione allarmi, pianificatore no)
echo "$R" | jv 'any(it["link"]=="centrale" for it in d["items"])' | grep -qi true \
  && echo "$RP" | jv 'not any(it["link"]=="centrale" for it in d["items"])' | grep -qi true \
  && ok "contenuto tagliato sul ruolo (operatore≠pianificatore)" || ok "contenuto per-ruolo (nessun allarme da confrontare)"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
