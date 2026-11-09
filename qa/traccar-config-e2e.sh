#!/usr/bin/env bash
# (b) Config server Traccar gestita DA YouWorkforce (superadmin -> Traccar PUT /api/server).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TR="${TR:-http://localhost:8083}"
TR_USER="${TR_USER:-op@acme.test}"; TR_PASS="${TR_PASS:-Passw0rd!}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
JS=$(mktemp)
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
CS=$(csrfh "$JS")
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c "import sys,json
for t in json.load(sys.stdin):
 if t['slug']=='acme':print(t['id'])")
S(){ curl -s -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' "$@"; }
# assicura connessione Traccar del tenant
S -X POST "$BASE/api/superadmin/tenants/$TID/traccar" -d "{\"base_url\":\"$TR\",\"username\":\"$TR_USER\",\"password\":\"$TR_PASS\"}" >/dev/null
# lettura config
S "$BASE/api/superadmin/tenants/$TID/traccar/config" | python -c "import sys,json;d=json.load(sys.stdin);ids=[c['id'] for c in d];assert 'registration' in ids and 'coordinateFormat' in ids and 'zoom' in ids;print('ok')" >/dev/null 2>&1 \
  && ok "lettura config server Traccar" || no "get" "-"
# scrittura: registration on + coordinateFormat + zoom
S -X POST "$BASE/api/superadmin/tenants/$TID/traccar/config" -d '{"entries":[{"id":"registration","value":"true"},{"id":"coordinateFormat","value":"dd"},{"id":"zoom","value":"7"}]}' | grep -q '"ok":true' \
  && ok "scrittura config Traccar da YouWorkforce" || no "set" "-"
# rilettura
S "$BASE/api/superadmin/tenants/$TID/traccar/config" | python -c "import sys,json;d=json.load(sys.stdin);r=next(c for c in d if c['id']=='registration');z=next(c for c in d if c['id']=='zoom');cf=next(c for c in d if c['id']=='coordinateFormat');assert r['value']==True,r;assert z['value']==7,z;assert cf['value']=='dd',cf;print('ok')" >/dev/null 2>&1 \
  && ok "riletta: registration/zoom/coordinateFormat applicati" || no "reread" "-"
# verifica diretta su Traccar
J=$(mktemp); curl -s -o /dev/null -c "$J" -X POST "$TR/api/session" --data-urlencode "email=$TR_USER" --data-urlencode "password=$TR_PASS"
curl -s -b "$J" "$TR/api/server" | python -c "import sys,json;d=json.load(sys.stdin);assert d['registration']==True and d['zoom']==7;print('ok')" >/dev/null 2>&1 \
  && ok "verifica diretta Traccar: scritto davvero" || no "direct" "-"
# ripristino registration off
S -X POST "$BASE/api/superadmin/tenants/$TID/traccar/config" -d '{"entries":[{"id":"registration","value":"false"}]}' >/dev/null
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
