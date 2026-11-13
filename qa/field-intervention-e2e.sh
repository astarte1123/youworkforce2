#!/usr/bin/env bash
# Lavoro sul campo — intervento end-to-end: report + allegati (foto/doc/firma) + firmatario.
# Perimetro modulo 'interventi', indipendente da Sicurezza Lavoratore.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TID=$(sql "SELECT id FROM app.tenants WHERE slug='$SLUG'" | tr -d '[:space:]')
# assicura il modulo attivo (idempotente)
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','interventi','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

OID=$(api -H 'content-type: application/json' -X POST "$BASE/api/orders" -d '{"title":"Riparazione cancello","instructions":"Motore bloccato"}' | jq "d.get('id','')")
[ -n "$OID" ] && ok "ordine creato" || no "ordine ko" "-"

# report + firmatario
RS=$(api -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID/report" -d '{"report":"Sostituito motore, testato","signed_by":"Luca Bianchi"}' | jq "d['ok']")
[ "$RS" = "True" ] && ok "report + firmatario salvati" || no "report ko" "$RS"
SB=$(sql "SELECT signed_by FROM app.work_orders WHERE id='$OID'")
echo "$SB" | grep -q "Luca Bianchi" && ok "firmatario persistito con signed_at" || no "signed_by ko" "$SB"

# allegato foto (bytes)
AID=$(printf 'FAKEPNGDATA' | api -H 'content-type: image/png' -X POST "$BASE/api/orders/$OID/attachments?kind=photo&filename=f.png&caption=cancello" --data-binary @- | jq "d['id']")
[ -n "$AID" ] && ok "allegato foto caricato" || no "allegato ko" "-"
# firma come allegato
printf 'SIGDATA' | api -o /dev/null -w "%{http_code}" -H 'content-type: image/png' -X POST "$BASE/api/orders/$OID/attachments?kind=signature&filename=firma.png" --data-binary @- | grep -q 200 && ok "firma (allegato kind=signature) caricata" || no "firma ko" "-"
# lista
N=$(api "$BASE/api/orders/$OID/attachments" | jq "len(d)")
[ "$N" = "2" ] && ok "lista allegati: 2 elementi" || no "lista ko" "$N"
# download bytes + content-type
GC=$(api -o /dev/null -w "%{http_code}:%{content_type}" "$BASE/api/orders/attachments/$AID")
echo "$GC" | grep -q "200:image/png" && ok "download allegato con content-type corretto" || no "download ko" "$GC"
# delete
api -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/orders/attachments/$AID" | grep -q 200 && ok "elimina allegato" || no "delete ko" "-"

# isolamento modulo: senza interventi -> 403
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='interventi'" >/dev/null
C403=$(api -o /dev/null -w "%{http_code}" "$BASE/api/orders/$OID/attachments")
[ "$C403" = 403 ] && ok "modulo disattivo -> allegati negati (403)" || no "gating ko" "$C403"
sql "UPDATE app.tenant_modules SET state='attivo' WHERE tenant_id='$TID' AND module_code='interventi'" >/dev/null

# teardown
sql "DELETE FROM app.work_order_attachments WHERE work_order_id='$OID'" >/dev/null
sql "DELETE FROM app.work_orders WHERE id='$OID'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
