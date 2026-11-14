#!/usr/bin/env bash
# Campo Slice 2 — Ronde e punzonature (modulo "giri"). Config giro+checkpoint (QR+GPS), esecuzione
# da PWA (start, punch qr OK/KO, punch gps dentro/fuori raggio), compliance e chiusura, gating
# modulo, isolamento tenant. Indipendente da interventi/workforce/loneworker.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
SLUG2="acme";  ADMIN2="admin@acme.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
TID2=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG2'")
# SOLO giri attivo (indipendenza): interventi/workforce/consuntivi spenti
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','giri','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code IN ('interventi','workforce','consuntivi')" >/dev/null
R=$RANDOM
HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
GU="guardia-$R@globex.loneworker.eu"
GUID=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$GU','$HASH','lavoratore') RETURNING id")

login(){ local em=$1 slug=$2 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $slug" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN" "$SLUG"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
A2J=$(login "$ADMIN2" "$SLUG2"); A2CS=$(awk '$6=="yw_csrf"{print $7}' "$A2J"|tail -1)
GJ=$(login "$GU" "$SLUG"); GCS=$(awk '$6=="yw_csrf"{print $7}' "$GJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
adm2(){ curl -s -b "$A2J" -H "x-tenant-slug: $SLUG2" -H "x-csrf-token: $A2CS" "$@"; }
g(){ curl -s -b "$GJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $GCS" "$@"; }

# 1) CONFIG: crea giro + 2 checkpoint (QR + GPS)
RID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes" -d '{"name":"Giro notturno '"$R"'","window_minutes":60}' | jq "d.get('id','')")
[ -n "$RID" ] && ok "crea giro" || no "route ko" "-"
C1=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/checkpoints" -d '{"name":"Portineria","kind":"qr","code":"QR-PORT-1"}' | jq "d.get('id','')")
C2=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/checkpoints" -d '{"name":"Magazzino","kind":"gps","lat":45.4642,"lon":9.1900,"radius_m":50}' | jq "d.get('id','')")
[ -n "$C1" ] && [ -n "$C2" ] && ok "crea 2 checkpoint (qr+gps)" || no "cp ko" "-"
# validazione: gps senza coord -> 400
[ "$(adm -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/checkpoints" -d '{"name":"X","kind":"gps"}')" = 400 ] && ok "gps senza coord -> 400" || no "val ko" "-"

# la guardia (lavoratore) NON configura
[ "$(g -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes" -d '{"name":"x"}')" = 403 ] && ok "lavoratore non configura (403)" || no "cfg acl ko" "-"

# 2) ESECUZIONE: la guardia avvia la ronda
ROUND=$(g -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/rounds" -d '{}' | jq "d.get('id','')")
[ -n "$ROUND" ] && ok "guardia avvia ronda" || no "round ko" "-"

# punch QR corretto -> ok
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$ROUND/punch" -d "{\"checkpoint_id\":\"$C1\",\"code\":\"QR-PORT-1\"}" | jq "d['ok']" | grep -qi true && ok "punch QR corretto = ok" || no "qr ok ko" "-"
# punch GPS fuori raggio (lontano) -> ko
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$ROUND/punch" -d "{\"checkpoint_id\":\"$C2\",\"lat\":45.50,\"lon\":9.30}" | jq "d['ok']" | grep -qi false && ok "punch GPS fuori raggio = ko" || no "gps far ko" "-"
# chiusura ora -> incompleto (manca C2 ok)
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$ROUND/close" -d '{}' | jq "d['status']" | grep -q incompleto && ok "chiusura con GPS mancante = incompleto" || no "close1 ko" "-"

# 3) nuova ronda completata correttamente
R2=$(g -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/rounds" -d '{}' | jq "d.get('id','')")
g -o /dev/null -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$R2/punch" -d "{\"checkpoint_id\":\"$C1\",\"code\":\"QR-PORT-1\"}" >/dev/null
# GPS dentro raggio (stesso punto)
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$R2/punch" -d "{\"checkpoint_id\":\"$C2\",\"lat\":45.4642,\"lon\":9.1900}" | jq "d['ok']" | grep -qi true && ok "punch GPS dentro raggio = ok" || no "gps near ko" "-"
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$R2/close" -d '{}' | jq "d['status']" | grep -q completato && ok "tutti i checkpoint ok = completato" || no "close2 ko" "-"
# dettaglio: done==total
g "$BASE/api/patrol-rounds/$R2" | jq "str(d['done'])+'/'+str(d['total'])" | grep -q "2/2" && ok "compliance 2/2" || no "compl ko" "-"

# 4) punch codice errato -> ko
R3=$(g -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID/rounds" -d '{}' | jq "d.get('id','')")
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$R3/punch" -d "{\"checkpoint_id\":\"$C1\",\"code\":\"SBAGLIATO\"}" | jq "d['ok']" | grep -qi false && ok "punch QR errato = ko" || no "qr bad ko" "-"

# 4b) CHECKPOINT RIUSABILE: importa C1 (QR) in un secondo giro e punzona lì
RID2=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes" -d '{"name":"Giro bis '"$R"'"}' | jq "d.get('id','')")
NEWCP=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID2/checkpoints/import" -d "{\"source_checkpoint_id\":\"$C1\"}" | jq "d.get('id','')")
[ -n "$NEWCP" ] && ok "checkpoint importato in un altro giro (riuso)" || no "import cp ko" "-"
RB=$(g -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes/$RID2/rounds" -d '{}' | jq "d.get('id','')")
g -H 'content-type: application/json' -X POST "$BASE/api/patrol-rounds/$RB/punch" -d "{\"checkpoint_id\":\"$NEWCP\",\"code\":\"QR-PORT-1\"}" | jq "d['ok']" | grep -qi true && ok "punzonatura sul checkpoint importato (stesso codice QR)" || no "import punch ko" "-"

# 5) ISOLAMENTO: acme non vede il giro di globex
SEEN=$(adm2 "$BASE/api/patrol-routes" 2>/dev/null | jq "len([r for r in d if r['id']=='$RID'])" 2>/dev/null || echo ERR)
[ "$SEEN" = 0 ] && ok "isolamento: acme non vede il giro (o modulo off)" || { [ "$SEEN" = ERR ] && ok "isolamento: acme modulo giri off (403)" || no "iso ko" "seen=$SEEN"; }

# 6) GATING: spengo giri -> config e esecuzione negate (403)
sql "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='giri'" >/dev/null
[ "$(adm -o /dev/null -w '%{http_code}' "$BASE/api/patrol-routes")" = 403 ] && ok "modulo giri off -> 403 (disaccoppiato)" || no "gating ko" "-"
sql "UPDATE app.tenant_modules SET state='attivo' WHERE tenant_id='$TID' AND module_code='giri'" >/dev/null

# teardown
sql "DELETE FROM app.patrol_routes WHERE id='$RID'" >/dev/null
sql "DELETE FROM app.patrol_routes WHERE id='${RID2:-00000000-0000-0000-0000-000000000000}'" >/dev/null 2>&1 || true
sql "DELETE FROM app.users WHERE id='$GUID'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
