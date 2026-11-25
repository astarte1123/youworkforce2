#!/usr/bin/env bash
# Chiave dei segreti gestita dal superadmin (decisione PO 25/11/2026, mig 0146).
#  - la chiave è nel DB solo sigillata (server + passphrase di recupero), stato visibile al superadmin
#  - passphrase di recupero: impostazione, cambio con la passphrase attuale (errata -> 403)
#  - i segreti (chiave AI) sono cifrati con la chiave attiva (enc:v3:<n>)
#  - rotazione: nuova chiave, segreti ricifrati, vecchia chiave disattivata (eliminata dalla pulizia)
#  - azzeramento (chiave persa) solo quando la chiave è bloccata
#  - sblocco con la passphrase (errata -> 403)
# ATTENZIONE: imposta sul DB di sviluppo la passphrase di recupero di test YW_E2E_RECOVERY
# (default «e2e-passphrase-di-recupero»). Non usare su un DB di produzione.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; P="${YW_E2E_RECOVERY:-e2e-passphrase-di-recupero}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sk$R"
TID=$(sa POST /api/superadmin/tenants "{\"slug\":\"$SLUG\",\"name\":\"Sk $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"assistant\"]}" | py "d.get('id','')")
trap 'sac DELETE "/api/superadmin/tenants/$TID" "{\"slug\":\"$SLUG\"}" >/dev/null; rm -rf "$J"' EXIT

S=$(sa GET /api/superadmin/secrets)
echo "$S" | py "d['state']=='attiva' and d['key_id'] is not None" | grep -q True && ok "chiave attiva e sigillata dal server ($(echo "$S" | py "d['seal_method']"))" || no "stato" "$S"
[ "$(q "SELECT count(*) FROM app.secret_keys WHERE active AND sealed_server IS NOT NULL")" = "1" ] && ok "nel DB solo la chiave sigillata" || no "db" "-"
if [ "$(echo "$S" | py "d['recovery_set']")" = "False" ]; then
  C=$(sac POST /api/superadmin/secrets/recovery '{"passphrase":"corta"}'); [ "$C" = "400" ] && ok "passphrase troppo corta: 400" || no "corta" "$C"
  C=$(sac POST /api/superadmin/secrets/recovery "{\"passphrase\":\"$P\"}"); [ "$C" = "200" ] && ok "passphrase di recupero impostata" || no "imposta" "$C"
fi
C=$(sac POST /api/superadmin/secrets/recovery "{\"passphrase\":\"$P\",\"current\":\"sbagliata-sbagliata\"}"); [ "$C" = "403" ] && ok "cambio con passphrase attuale errata: 403" || no "cambio errato" "$C"
C=$(sac POST /api/superadmin/secrets/recovery "{\"passphrase\":\"$P\",\"current\":\"$P\"}"); [ "$C" = "200" ] && ok "cambio con la passphrase attuale: 200" || no "cambio" "$C"

K0=$(sa GET /api/superadmin/secrets | py "d['key_id']")
sa POST "/api/superadmin/tenants/$TID/ai" '{"base_url":"http://localhost:9099/","api_key":"sk-e2e-segreto","model":"youworker-1"}' >/dev/null
V=$(q "SELECT api_key FROM app.ai_connections WHERE tenant_id='$TID'")
case "$V" in enc:v3:$K0:*) ok "chiave AI cifrata con la chiave attiva (enc:v3:$K0)";; *) no "cifratura" "$V";; esac
echo "$V" | grep -q "sk-e2e-segreto" && no "chiaro" "-" || ok "il segreto non compare in chiaro"

C=$(sac POST /api/superadmin/secrets/rotate '{"passphrase":"sbagliata-sbagliata"}'); [ "$C" = "403" ] && ok "rotazione con passphrase errata: 403" || no "rot errata" "$C"
K1=$(sa POST /api/superadmin/secrets/rotate "{\"passphrase\":\"$P\"}" | py "d['key_id']")
[ "$K1" = "$((K0+1))" ] && ok "chiave ruotata ($K0 -> $K1)" || no "rotazione" "$K1"
case "$(q "SELECT api_key FROM app.ai_connections WHERE tenant_id='$TID'")" in enc:v3:$K1:*) ok "segreti ricifrati con la nuova chiave";; *) no "ricifra" "-";; esac
# la vecchia chiave resta (disattivata) qualche minuto, per le altre istanze; poi la pulizia la elimina
[ "$(q "SELECT count(*) FROM app.secret_keys WHERE key_id=$K0 AND NOT active")$(q "SELECT count(*) FROM app.secret_keys WHERE active")" = "11" ] && ok "vecchia chiave disattivata, una sola attiva" || no "vecchia" "-"
C=$(sac POST /api/superadmin/secrets/reset '{"confirm":"AZZERA"}'); [ "$C" = "409" ] && ok "azzeramento rifiutato con la chiave attiva (409)" || no "reset" "$C"

C=$(sac POST /api/superadmin/secrets/unlock '{"passphrase":"sbagliata-sbagliata"}'); [ "$C" = "403" ] && ok "sblocco con passphrase errata: 403" || no "unlock errato" "$C"
C=$(sac POST /api/superadmin/secrets/unlock "{\"passphrase\":\"$P\"}"); [ "$C" = "200" ] && ok "sblocco con la passphrase: chiave risigillata per questo server" || no "unlock" "$C"
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" "$BASE/api/superadmin/secrets"); [ "$C" = "403" ] || [ "$C" = "401" ] && ok "admin del tenant: nessun accesso ($C)" || no "acl" "$C"
sa GET /api/superadmin/system | py "[c['severity'] for c in d['checks'] if c['kind']=='secrets'][0]" | grep -q ok && ok "pannello di sistema: cifratura ok" || no "check" "-"

echo "secret-key: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
