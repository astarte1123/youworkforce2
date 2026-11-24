#!/usr/bin/env bash
# FE-1: «I miei allarmi» del reperibile.
#  - /api/incidents segnala per ogni incidente se l'utente è stato avvisato (notified_me) o se gli è
#    assegnato (assigned_to_me): la pagina mostra «Miei» e «Tutti aperti»
#  - il reperibile può aprire il flusso in tempo reale (/api/stream): la pagina si aggiorna da sola
# Tenant usa-e-getta (eliminato a fine suite). Consegne simulate/sink, nessun recapito reale.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sqlx(){ docker exec -i ywdb psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -tAc "$1"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="mia$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Miei $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
RID=$(call A POST /api/users "{\"email\":\"rep@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"RepPass99\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
call A POST "/api/users/$RID" '{"phone":"+390000000777"}' >/dev/null
GID=$(call A POST /api/groups '{"name":"Turno"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
call A POST "/api/groups/$GID/members" "{\"user_id\":\"$RID\",\"position\":1}" >/dev/null
I1=$(sqlx "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) VALUES('$TID','sos','aperto',3,'mia-$R-1',now(),'$GID') RETURNING id" | head -1 | tr -d '[:space:]')
I2=$(sqlx "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) VALUES('$TID','man_down','aperto',3,'mia-$R-2',now()) RETURNING id" | head -1 | tr -d '[:space:]')
call A POST /api/escalation/delivery/run '{}' >/dev/null
login P "rep@$SLUG.test" RepPass99
F=$(call P GET /api/incidents | python -c "import sys,json;d={i['id']:i for i in json.load(sys.stdin)};print(d['$I1']['notified_me'],d['$I2']['notified_me'],d['$I2']['assigned_to_me'])")
[ "$F" = "True False False" ] && ok "avvisato per il SOS del suo gruppo, non per l'altro allarme" || no "notified_me" "$F"
call A POST "/api/incidents/$I2/assign" "{\"user_id\":\"$RID\"}" >/dev/null
F=$(call P GET /api/incidents | python -c "import sys,json;d={i['id']:i for i in json.load(sys.stdin)};print(d['$I2']['assigned_to_me'])")
[ "$F" = "True" ] && ok "incidente assegnato: assigned_to_me" || no "assigned_to_me" "$F"
H=$(curl -s -m 3 -o /dev/null -w "%{http_code} %{content_type}" -b "$J/P" -H "x-tenant-slug: $SLUG" "$BASE/api/stream")
case "$H" in 200*text/event-stream*) ok "il reperibile apre il flusso in tempo reale";; *) no "stream" "$H";; esac

echo "miei-allarmi: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
