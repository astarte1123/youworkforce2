#!/usr/bin/env bash
# SA-3: stato della consegna allarmi (reale / prova / simulata) visibile e controllato.
#  - il pannello di sistema del superadmin riporta il modo e un controllo «delivery» se non è reale
#  - /api/delivery-status: chi segue gli allarmi lo vede (avviso in Centrale), un lavoratore no
#  - il modo deriva dalla configurazione: con YW_ESC_SINK_URL e senza Twilio live -> «prova»
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ds$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ds $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"loneworker\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" "$BASE$2"; }
login A "a@$SLUG.test" DirectPass9
call A POST /api/users "{\"email\":\"w@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" >/dev/null
login W "w@$SLUG.test" WorkPass9

SYS=$(curl -s -b "$J/sa" "$BASE/api/superadmin/system")
MODE=$(echo "$SYS" | py "d['delivery']['mode']")
[ -n "$MODE" ] && ok "pannello di sistema: modo di consegna «$MODE»" || no "snapshot" "$(echo "$SYS" | head -c 200)"
if [ "$MODE" = "reale" ]; then
  echo "$SYS" | py "[c['severity'] for c in d['checks'] if c['kind']=='delivery'][0]" | grep -q ok && ok "consegna reale: controllo ok" || no "check reale" "-"
else
  echo "$SYS" | py "[c['severity'] for c in d['checks'] if c['kind']=='delivery'][0]" | grep -qE "warn|bad" \
    && ok "consegna non reale: controllo «delivery» in allerta" || no "check delivery" "-"
fi
D=$(call A GET /api/delivery-status)
echo "$D" | py "d['mode']=='$MODE' and (d['real']==(d['mode']=='reale')) and len(d['label'])>10" | grep -q True \
  && ok "admin del tenant: stato consegna con spiegazione" || no "tenant" "$D"
C=$(code W /api/delivery-status); [ "$C" = "403" ] && ok "lavoratore: 403" || no "lavoratore" "$C"
if [ -n "${YW_EXPECT_DELIVERY:-}" ]; then [ "$MODE" = "$YW_EXPECT_DELIVERY" ] && ok "modo atteso $YW_EXPECT_DELIVERY" || no "atteso" "$MODE"; fi

echo "delivery-status: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
