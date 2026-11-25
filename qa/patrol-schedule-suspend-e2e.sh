#!/usr/bin/env bash
# Piani di ronda sospendibili (mig 0164): sospeso = nessuna ronda dovuta; riattivato = le ronde
# dovute ripartono da adesso (niente ronde mancate arretrate del periodo sospeso). Eliminare o
# sospendere un piano inesistente: 404. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ps$R"
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ps $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"giri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
RT=$(call POST /api/patrol-routes '{"name":"Perimetro"}' | py "d['id']")
SID=$(call POST /api/patrol-schedules "{\"route_id\":\"$RT\",\"from_time\":\"00:00\",\"to_time\":\"23:59\",\"every_minutes\":30}" | py "d['id']")
# il piano creato ora conta da ora (created_at): retrodatiamo la creazione per avere orari «mancanti» oggi
docker exec ywdb psql -U postgres -d youworkforce -qc "UPDATE app.patrol_schedules SET created_at=created_at-interval '2 days', active_since=active_since-interval '2 days' WHERE id='$SID'"
dash(){ call GET /api/patrol-dashboard | py "$1"; }
M0=$(dash "sum(1 for s in d['slots'] if s['status']=='mancante')")
[ "$M0" -ge 1 ] && ok "piano attivo: oggi ci sono ronde dovute non svolte ($M0)" || no "slot mancanti" "$M0"

[ "$(code POST "/api/patrol-schedules/$SID" '{"active":false}')" = "200" ] && ok "piano sospeso" || no "sospendi" "-"
call GET /api/patrol-schedules | py "[s['active'] for s in d if s['id']=='$SID'][0]" | grep -q False && ok "l'elenco mostra il piano sospeso" || no "elenco" "-"
[ "$(dash "len(d['slots'])")" = "0" ] && ok "piano sospeso: nessuna ronda dovuta nel cruscotto" || no "slot da sospeso" "$(dash "len(d['slots'])")"

[ "$(code POST "/api/patrol-schedules/$SID" '{"active":true}')" = "200" ] && ok "piano riattivato" || no "riattiva" "-"
[ "$(dash "sum(1 for s in d['slots'] if s['status']=='mancante')")" = "0" ] && ok "riattivato: gli orari del periodo sospeso non risultano mancati" || no "arretrati" "$(dash "[s for s in d['slots'] if s['status']=='mancante'][:3]")"

NX=00000000-0000-0000-0000-000000000000
[ "$(code DELETE "/api/patrol-schedules/$NX")" = "404" ] && ok "eliminare un piano inesistente: 404" || no "delete 404" "-"
[ "$(code POST "/api/patrol-schedules/$NX" '{"active":false}')" = "404" ] && ok "sospendere un piano inesistente: 404" || no "toggle 404" "-"
[ "$(code DELETE "/api/patrol-schedules/$SID")" = "200" ] && ok "piano eliminato" || no "delete" "-"
echo "patrol-schedule-suspend: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
