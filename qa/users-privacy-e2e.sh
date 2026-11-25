#!/usr/bin/env bash
# TA-10: l'elenco utenti non espone più a tutti email, ruolo e telefono dei colleghi.
#  - lavoratore senza permessi: vede solo sé stesso
#  - chi coordina senza pianificare (auditor): elenco senza telefoni
#  - chi pianifica, gestisce gli allarmi (operatore) o gli utenti (admin): elenco con telefoni
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="up$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Up $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"loneworker\",\"interventi\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"$2\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
UW=$(mk w lavoratore); UP=$(mk p pianificatore); UO=$(mk o operatore); UA=$(mk au auditor)
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.users SET phone='+393330000001' WHERE tenant_id='$TID'" >/dev/null
for u in w p o au; do login $u "$u@$SLUG.test" WorkPass9; done
L=$(call w GET /api/users); echo "$L" | py "len(d)==1 and d[0]['email']=='w@$SLUG.test' and d[0]['phone'] is not None" | grep -q True \
  && ok "lavoratore: vede solo sé stesso (con il proprio telefono)" || no "lavoratore" "$L"
L=$(call p GET /api/users); echo "$L" | py "len(d)>=4 and all(u['phone'] for u in d)" | grep -q True \
  && ok "pianificatore: elenco con telefoni (contattare un tecnico sul campo, decisione PO)" || no "pianificatore" "$(echo "$L" | head -c 200)"
# chi coordina senza pianificare (es. auditor): elenco senza telefoni
L=$(call au GET /api/users); echo "$L" | py "len(d)>=4 and all(u['phone'] is None for u in d if u['email']!='au@$SLUG.test')" | grep -q True \
  && ok "auditor: elenco senza telefoni dei colleghi" || no "auditor" "$(echo "$L" | head -c 200)"
L=$(call o GET /api/users); echo "$L" | py "len(d)>=4 and all(u['phone'] for u in d)" | grep -q True \
  && ok "operatore di centrale: telefoni visibili (deve poter chiamare chi è in pericolo)" || no "operatore" "-"
L=$(call A GET /api/users); echo "$L" | py "len(d)>=4 and all(u['phone'] for u in d)" | grep -q True && ok "amministratore: elenco completo" || no "admin" "-"
echo "users-privacy: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
