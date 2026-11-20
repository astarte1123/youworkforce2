#!/usr/bin/env bash
# Ondata 1 superadmin: onboarding admin via INVITO monouso (nessuna password nota al superadmin)
# + validazione slug/email + password diretta (demo).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }

echo "== Superadmin — invito tenant admin =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
S(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
R=$(python -c 'import random;print(random.randint(100000,999999))')
SLUG="inv$R"; EMAIL="admin@$SLUG.test"

say "1 crea tenant in modalità INVITO (senza password)"
RESP=$(S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$SLUG\",\"name\":\"Invite $R\",\"admin_email\":\"$EMAIL\"}")
TOK=$(echo "$RESP" | jq1 "print(json.load(sys.stdin).get('invite_token') or '')")
[ -n "$TOK" ] && ok || no "no invite_token: $RESP"

say "2 check invito valido (email combacia)"
E=$(curl -s "$BASE/api/invite/check?slug=$SLUG&token=$TOK" | jq1 "d=json.load(sys.stdin);print(d.get('valid'),d.get('email'))")
[ "$E" = "True $EMAIL" ] && ok || no "$E"

say "3 login admin PRIMA dell'accettazione -> 401 (nessuna password nota)"
c=$(code -X POST $BASE/api/login -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}")
[ "$c" = "401" ] && ok || no "got $c"

say "4 accept con password corta -> 400"
c=$(code -X POST $BASE/api/invite/accept -H 'content-type: application/json' -d "{\"slug\":\"$SLUG\",\"token\":\"$TOK\",\"password\":\"short\"}")
[ "$c" = "400" ] && ok || no "got $c"

say "5 accept con password valida -> ok"
c=$(code -X POST $BASE/api/invite/accept -H 'content-type: application/json' -d "{\"slug\":\"$SLUG\",\"token\":\"$TOK\",\"password\":\"SuperSecret9\"}")
[ "$c" = "200" ] && ok || no "got $c"

say "6 login admin con la NUOVA password -> 200"
c=$(code -X POST $BASE/api/login -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"SuperSecret9\"}")
[ "$c" = "200" ] && ok || no "got $c"

say "7 riuso dello stesso invito -> 409 (monouso)"
c=$(code -X POST $BASE/api/invite/accept -H 'content-type: application/json' -d "{\"slug\":\"$SLUG\",\"token\":\"$TOK\",\"password\":\"AnotherPass9\"}")
[ "$c" = "409" ] && ok || no "got $c"

say "8 check invito dopo l'uso -> non valido"
V=$(curl -s "$BASE/api/invite/check?slug=$SLUG&token=$TOK" | jq1 "print(json.load(sys.stdin).get('valid'))")
[ "$V" = "False" ] && ok || no "$V"

say "9 token invito errato -> non valido"
V=$(curl -s "$BASE/api/invite/check?slug=$SLUG&token=deadbeef" | jq1 "print(json.load(sys.stdin).get('valid'))")
[ "$V" = "False" ] && ok || no "$V"

say "10 slug non valido -> 400"
c=$(code -X POST $BASE/api/superadmin/tenants -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -d '{"slug":"!!","name":"X","admin_email":"a@b.co"}')
[ "$c" = "400" ] && ok || no "got $c"

say "11 email admin non valida -> 400"
c=$(code -X POST $BASE/api/superadmin/tenants -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -d "{\"slug\":\"e2e$R\",\"name\":\"X\",\"admin_email\":\"nope\"}")
[ "$c" = "400" ] && ok || no "got $c"

say "12 modalità password diretta -> login immediato"
S2="pwd$R"; E2="admin@$S2.test"
S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$S2\",\"name\":\"Pwd $R\",\"admin_email\":\"$E2\",\"admin_password\":\"DirectPass9\"}" -o /dev/null
c=$(code -X POST $BASE/api/login -H 'content-type: application/json' -H "x-tenant-slug: $S2" -d "{\"email\":\"$E2\",\"password\":\"DirectPass9\"}")
[ "$c" = "200" ] && ok || no "got $c"

say "13 pacchetto 'campo' -> moduli attivati alla creazione"
S3="plan$R"
RID=$(S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$S3\",\"name\":\"Plan $R\",\"admin_email\":\"admin@$S3.test\",\"plan\":\"campo\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
ACT=$(S $BASE/api/superadmin/tenants/$RID/modules | jq1 "import sys,json;d=json.load(sys.stdin);print(sorted(m['code'] for m in d if m.get('state')=='attivo'))")
echo "$ACT" | grep -q "interventi" && echo "$ACT" | grep -q "cantieri" && echo "$ACT" | grep -q "workforce" && ok || no "$ACT"

say "14 licenze impostate alla creazione (mobile=5, iot=10)"
S4="lic$R"
LID=$(S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$S4\",\"name\":\"Lic $R\",\"admin_email\":\"admin@$S4.test\",\"mobile_app_licenses\":5,\"iot_device_licenses\":10}" | jq1 "print(json.load(sys.stdin).get('id',''))")
L=$(S $BASE/api/superadmin/tenants | jq1 "t=next(t for t in json.load(sys.stdin) if t['id']=='$LID');print(t.get('mobile_app_licenses'),t.get('iot_device_licenses'))")
[ "$L" = "5 10" ] && ok || no "$L"

say "15 scenario + demo alla creazione -> applicato (iot attivo)"
S5="scn$R"
NID=$(S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$S5\",\"name\":\"Scn $R\",\"admin_email\":\"admin@$S5.test\",\"plan\":\"iot\",\"scenario\":\"smart_energy\",\"with_demo\":true}" | jq1 "print(json.load(sys.stdin).get('id',''))")
SA=$(S $BASE/api/superadmin/tenants/$NID/scenarios | jq1 "import sys,json;d=json.load(sys.stdin);print(any(s.get('code')=='smart_energy' for s in d))")
[ "$SA" = "True" ] && ok || no "scenario non applicato: $SA"

# teardown: rimuove i tenant di test (nessun endpoint delete fino a Ondata 3)
if command -v docker >/dev/null 2>&1; then
  docker exec ywdb psql -U postgres -d youworkforce -tAc \
    "DELETE FROM app.tenants WHERE slug LIKE '%$R';" >/dev/null 2>&1 || true
fi

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
