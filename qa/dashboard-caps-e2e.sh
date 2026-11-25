#!/usr/bin/env bash
# IO-16 (decisione PO 26/11/2026): widget del cruscotto filtrati per PERMESSO, non per nome del ruolo;
# nuovo widget IoT. Tenant usa-e-getta (Lone Worker + IoT + Interventi).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="dc$R"
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Dc $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"loneworker\",\"iot\",\"interventi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ rm -f "$J/$1"; curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$1@$SLUG.test\",\"password\":\"$2\"}"; }
as(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login a DirectPass9
for r in lavoratore operatore reperibile; do as a POST /api/users "{\"email\":\"$r@$SLUG.test\",\"role\":\"$r\",\"password\":\"WorkPass9\"}" >/dev/null; done
W(){ login "$1" WorkPass9; as "$1" GET /api/dashboard/layout | py "sorted(w['key'] for w in d['widgets'])"; }

L=$(W lavoratore); echo "$L" | grep -q "'orders'" && ! echo "$L" | grep -q "'incidents'" && ! echo "$L" | grep -q "'iot'" \
  && ok "lavoratore: i propri interventi (permesso «lavoro personale»), niente allarmi né IoT" || no "lavoratore" "$L"
O=$(W operatore); echo "$O" | grep -q "'iot'" && echo "$O" | grep -q "'incidents'" && ! echo "$O" | grep -q "'orders'" \
  && ok "operatore: allarmi e nuovo widget IoT, niente interventi" || no "operatore" "$O"
P=$(W reperibile); ! echo "$P" | grep -q "'incidents'" && ok "reperibile senza permesso di lettura della Centrale: niente widget allarmi" || no "reperibile prima" "$P"
# il permesso, non il ruolo: dare «incidents.view» al reperibile fa comparire il widget
as a POST /api/capabilities '{"role":"reperibile","capability":"incidents.view","granted":true}' >/dev/null
P=$(W reperibile); echo "$P" | grep -q "'incidents'" && ok "dato il permesso incidents.view al reperibile: compare il widget allarmi" || no "reperibile dopo" "$P"
as a POST /api/capabilities '{"role":"lavoratore","capability":"work.own","granted":false}' >/dev/null
L=$(W lavoratore); ! echo "$L" | grep -q "'orders'" && ok "tolto «lavoro personale» al lavoratore: sparisce il widget interventi" || no "lavoratore senza work.own" "$L"
login a DirectPass9
as a GET /api/dashboard/layout | py "'iot' in [w['key'] for w in d['widgets']]" | grep -q True && ok "amministratore: widget IoT presente" || no "admin iot" "-"
as a GET /api/iot/monitor | py "'summary' in d" | grep -q True && ok "il widget IoT legge il riepilogo del monitoraggio" || no "iot monitor" "-"
echo "dashboard-caps: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
