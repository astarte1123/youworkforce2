#!/usr/bin/env bash
# Config trasporto notifiche GoAlert gestita DA YouWorkforce (superadmin -> GoAlert via GraphQL setConfig).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; GA="${GA:-http://localhost:8082}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
JS=$(mktemp); JA=$(mktemp)
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CS=$(csrfh "$JS")
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c "import sys,json
for t in json.load(sys.stdin):
 if t['slug']=='acme':print(t['id'])")
S(){ curl -s -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' "$@"; }
URL="http://acme-$(date +%s).example"

# lettura config (Twilio presente, segreti mascherati)
S "$BASE/api/superadmin/tenants/$TID/goalert/config" | python -c "import sys,json;d=json.load(sys.stdin);ids=[c['id'] for c in d];assert 'Twilio.Enable' in ids and 'SMTP.From' in ids and 'Webhook.AllowedURLs' in ids;t=next(c for c in d if c['id']=='Twilio.AuthToken');assert t['secret'] and t['value'] is None;print('ok')" >/dev/null 2>&1 \
  && ok "lettura config trasporto (Twilio/SMTP/Webhook, segreti mascherati)" || no "get config" "-"

# scrittura: URL pubblico + mittente SMTP + segreto Slack
S -X POST "$BASE/api/superadmin/tenants/$TID/goalert/config" -d "{\"entries\":[{\"id\":\"General.PublicURL\",\"value\":\"$URL\"},{\"id\":\"SMTP.From\",\"value\":\"noreply@acme.test\"},{\"id\":\"Slack.SigningSecret\",\"value\":\"sig-123\"}]}" | grep -q '"ok":true' \
  && ok "scrittura config da YouWorkforce (URL, SMTP.From, segreto Slack)" || no "set config" "-"

# rilettura: valori applicati, segreto impostato
S "$BASE/api/superadmin/tenants/$TID/goalert/config" | python -c "import sys,json;d=json.load(sys.stdin);u=next(c for c in d if c['id']=='General.PublicURL');f=next(c for c in d if c['id']=='SMTP.From');sk=next(c for c in d if c['id']=='Slack.SigningSecret');assert u['value']=='$URL',u;assert f['value']=='noreply@acme.test',f;assert sk['set']==True,sk;print('ok')" >/dev/null 2>&1 \
  && ok "riletta: valori applicati + segreto impostato" || no "reread" "-"

# verifica diretta su GoAlert (scritto davvero là)
J=$(mktemp); curl -s -o /dev/null -c "$J" -X POST "$GA/api/v2/identity/providers/basic" -H "Referer: $GA/" -H "Origin: $GA" --data-urlencode "username=admin" --data-urlencode "password=goalertadmin1"
curl -s -b "$J" -X POST "$GA/api/graphql" -H 'content-type: application/json' -H "Referer: $GA/" -d '{"query":"{config{id value}}"}' | grep -q "$URL" \
  && ok "verifica diretta GoAlert: config scritta davvero" || no "goalert direct" "-"

# segreto vuoto non sovrascrive
S -X POST "$BASE/api/superadmin/tenants/$TID/goalert/config" -d '{"entries":[{"id":"Slack.SigningSecret","value":""},{"id":"General.PublicURL","value":"'"$URL"'"}]}' | grep -q '"ok":true' \
  && S "$BASE/api/superadmin/tenants/$TID/goalert/config" | python -c "import sys,json;d=json.load(sys.stdin);sk=next(c for c in d if c['id']=='Slack.SigningSecret');assert sk['set']==True,sk;print('ok')" >/dev/null 2>&1 \
  && ok "segreto vuoto ignorato (resta impostato)" || no "secret skip" "-"

# RBAC: non-superadmin negato
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/superadmin/tenants/$TID/goalert/config")
{ [ "$C" = "401" ] || [ "$C" = "403" ]; } && ok "non-superadmin negato ($C)" || no "rbac" "$C"

echo "== $PASS passed, $FAIL failed =="; exit $FAIL
