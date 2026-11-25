#!/usr/bin/env bash
# SA-11 (decisione PO 25/11/2026): canali di consegna esterni = eccezioni dichiarate, configurati
# solo dal superadmin; i segreti cifrati con la chiave dei segreti e mai mostrati.
# Usa solo canali innocui (mittente e token Slack fittizio) e li toglie alla fine.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
HAD_FROM=$(q "SELECT count(*) FROM app.delivery_channels WHERE key='YW_ESC_FROM'")
cleanup(){ [ "$HAD_FROM" = "0" ] && sa -o /dev/null -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_FROM","value":null}'
  sa -o /dev/null -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_SLACK_TOKEN","value":null}'; rm -rf "$J"; }
trap cleanup EXIT

G=$(sa "$BASE/api/superadmin/channels")
echo "$G" | py "len(d['channels'])==6 and 'delivery' in d" | grep -q True && ok "elenco dei 6 canali con lo stato della consegna" || no "elenco" "$G"
C=$(sac -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_FROM","value":"12345"}'); [ "$C" = "400" ] && ok "mittente non internazionale rifiutato (400)" || no "e164" "$C"
C=$(sac -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ALTRO","value":"x"}'); [ "$C" = "400" ] && ok "canale sconosciuto rifiutato (400)" || no "sconosciuto" "$C"
sa -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_FROM","value":"+390200000001"}' \
  | py "[c for c in d['channels'] if c['key']=='YW_ESC_FROM'][0]['source']=='superadmin'" | grep -q True && ok "mittente impostato dal superadmin" || no "from" "-"
R=$(sa -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_SLACK_TOKEN","value":"xoxb-e2e-segreto"}')
echo "$R" | py "[c for c in d['channels'] if c['key']=='YW_ESC_SLACK_TOKEN'][0]['value'] is None" | grep -q True && ok "il token non viene mai restituito" || no "token visibile" "$R"
case "$(q "SELECT value FROM app.delivery_channels WHERE key='YW_ESC_SLACK_TOKEN'")" in enc:v3:*) ok "token cifrato nel DB (enc:v3)";; *) no "cifratura" "-";; esac
echo "$R" | grep -q "xoxb-e2e-segreto" && no "token in chiaro nella risposta" "-" || ok "nessun segreto in chiaro nella risposta"
[ "$(q "SELECT count(*) FROM app.audit_log WHERE action='delivery_channel_set' AND detail::text LIKE '%xoxb%'")" = "0" ] && ok "il registro attività non contiene il valore" || no "audit" "-"
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H 'x-tenant-slug: acme' "$BASE/api/superadmin/channels"); { [ "$C" = "401" ] || [ "$C" = "403" ]; } && ok "admin del tenant: nessun accesso ($C)" || no "acl" "$C"
sa -X POST "$BASE/api/superadmin/channels" -d '{"key":"YW_ESC_SLACK_TOKEN","value":null}' \
  | py "[c for c in d['channels'] if c['key']=='YW_ESC_SLACK_TOKEN'][0]['source']!='superadmin'" | grep -q True && ok "valore tolto: torna quello di installazione" || no "togli" "-"
echo "delivery-channels: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
