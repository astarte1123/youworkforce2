#!/usr/bin/env bash
# Fase A+B+C: catalogo ricercabile, asset, wizard provisioning, assegnazione utente/asset,
# "attivo al primo fix".
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="ONB-$RUN"

JA=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CM=$(csrfh "$JM")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
MARIO=$(A "$BASE/api/users" | python -c 'import sys,json;print(next(u["id"] for u in json.load(sys.stdin) if u["email"].startswith("mario@")))')

# A — catalogo
A "$BASE/api/catalog/devices?q=teltonika" | grep -q 'teltonika-tmt250' && ok "ricerca catalogo (teltonika)" || no "catalog search" "-"
A "$BASE/api/catalog/devices?category=personale" | python -c 'import sys,json;d=json.load(sys.stdin);assert all(x["category"]=="personale" for x in d) and len(d)>=3;print("ok")' >/dev/null 2>&1 \
  && ok "filtro per scopo (personale)" || no "catalog filter" "-"
A "$BASE/api/catalog/facets" | python -c 'import sys,json;d=json.load(sys.stdin);assert "Teltonika" in d["manufacturers"];assert "veicolo" in d["categories"];print("ok")' >/dev/null 2>&1 \
  && ok "facets produttori/categorie" || no "facets" "-"

# C — asset
AID=$(A -X POST "$BASE/api/assets" -d "{\"name\":\"Furgone $RUN\",\"kind\":\"veicolo\",\"identifier\":\"AB123CD\"}" | jget 'd["id"]')
[ -n "$AID" ] && ok "asset creato" || no "asset" "$AID"

# B — provisioning (modello personale -> assegnato a mario)
R=$(A -X POST "$BASE/api/devices/provision" -d "{\"model_code\":\"teltonika-tmt250\",\"external_id\":\"$EXT\",\"label\":\"Badge Mario\",\"user_id\":\"$MARIO\"}")
DID=$(echo "$R" | jget 'd["device_id"]')
echo "$R" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["step"]=="in_attesa_primo_fix",d;assert d["install"]["protocol"]=="teltonika",d;assert d["install"]["port"]==5027,d;print("ok")' >/dev/null 2>&1 \
  && ok "provision: step+istruzioni installazione (teltonika:5027)" || no "provision" "$R"
# device in lista con modello/capacita'/stato
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert x['manufacturer']=='Teltonika',x;assert 'sos' in (x['capabilities'] or []),x;assert x['prov_step']=='in_attesa_primo_fix',x;print('ok')" >/dev/null 2>&1 \
  && ok "device mostra modello, capacita', stato onboarding" || no "device enriched" "-"

# attivo al primo fix
curl -s -o /dev/null -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"onb1\",\"latitude\":45.46,\"longitude\":9.19,\"fixTime\":\"2026-11-10T10:00:00Z\"}}"
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert x['prov_step']=='attivo',x;print('ok')" >/dev/null 2>&1 \
  && ok "attivo al primo fix" || no "active" "-"

# assegnazione a asset (XOR)
A -X POST "$BASE/api/devices/$DID/assign" -d "{\"asset_id\":\"$AID\"}" | grep -q '"ok":true' && ok "riassegnato ad asset" || no "assign asset" "-"
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert x['asset_name'] and 'Furgone' in x['asset_name'],x;print('ok')" >/dev/null 2>&1 \
  && ok "device ora legato all'asset" || no "asset link" "-"
C=$(A -o /dev/null -w '%{http_code}' -X POST "$BASE/api/devices/$DID/assign" -d "{\"user_id\":\"$MARIO\",\"asset_id\":\"$AID\"}")
[ "$C" = "400" ] && ok "assegnazione utente+asset insieme -> 400 (XOR)" || no "xor" "$C"

# RBAC: operatore/lavoratore non fa provisioning
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' -X POST "$BASE/api/devices/provision" -d "{\"model_code\":\"teltonika-tmt250\",\"external_id\":\"x$RUN\"}")
[ "$C" = "403" ] && ok "lavoratore non fa provisioning (403)" || no "rbac" "$C"

# cleanup
A -X DELETE "$BASE/api/devices/$DID" >/dev/null; A -X DELETE "$BASE/api/assets/$AID" >/dev/null
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
