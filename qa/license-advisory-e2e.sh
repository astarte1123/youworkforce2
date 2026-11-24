#!/usr/bin/env bash
# SA-6 (decisione PO): licenze SOLO indicative. Oltre il limite nulla è bloccato: l'utente è creato,
# la risposta e l'amministratore (banner da /api/me) riportano il superamento. I ruoli personalizzati
# non contano come utenti; l'app personale del lavoratore non conta come dispositivo.
# Tenant usa-e-getta, eliminato a fine suite.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="lic$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Lic $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\",\"mobile_app_licenses\":1,\"iot_device_licenses\":0}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
nwarn(){ python -c "import sys,json;print(len(json.load(sys.stdin).get('license_warnings',[])))"; }
login A "a@$SLUG.test" DirectPass9

N=$(call A POST /api/users "{\"email\":\"w1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"Work1Pass\"}" | nwarn)
[ "$N" = "0" ] && ok "primo lavoratore entro la licenza: nessun avviso" || no "w1" "$N"
RW=$(call A POST /api/users "{\"email\":\"w2@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"Work2Pass\"}")
echo "$RW" | python -c "import sys,json;d=json.load(sys.stdin);w=d.get('license_warnings',[]);sys.exit(0 if d.get('id') and any(x['kind']=='mobile' and x['used']==2 and x['limit']==1 for x in w) else 1)" \
  && ok "oltre licenza: utente CREATO comunque, con avviso 2 su 1" || no "w2" "$RW"
call A POST /api/users "{\"email\":\"op@$SLUG.test\",\"role\":\"operatore\",\"password\":\"OperPass9\"}" >/dev/null
OPID=$(call A GET /api/users | python -c "import sys,json;print([u['id'] for u in json.load(sys.stdin) if u['email']=='op@$SLUG.test'][0])")
RID=$(call A POST /api/custom-roles '{"name":"Extra","capabilities":["plan"]}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
call A POST "/api/custom-roles/$RID/users" "{\"user_id\":\"$OPID\"}" >/dev/null
U=$(call A GET /api/me | python -c "import sys,json;print([x['used'] for x in json.load(sys.stdin)['license_notice'] if x['kind']=='mobile'])")
[ "$U" = "[2]" ] && ok "ruolo personalizzato e operatore non contano come utenti dell'app (2)" || no "conteggio" "$U"
K=$(call A GET /api/me | python -c "import sys,json;print(sorted(x['kind'] for x in json.load(sys.stdin)['license_notice']))")
[ "$K" = "['mobile']" ] && ok "avviso all'amministratore in /api/me (solo app: nessun dispositivo censito)" || no "notice" "$K"
login W "w1@$SLUG.test" Work1Pass
L=$(call W GET /api/me | python -c "import sys,json;print(len(json.load(sys.stdin)['license_notice']))")
[ "$L" = "0" ] && ok "il lavoratore non vede l'avviso licenze" || no "notice worker" "$L"

echo "license-advisory: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
