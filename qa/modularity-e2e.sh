#!/usr/bin/env bash
# Modularità per-tenant: un modulo spento rifiuta anche i NUOVI ingress (contratto),
# e il superadmin lo accende/spegne per tenant SENZA deploy, con effetto immediato.
# Usa globex (loneworker attivo; iot spento).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }

code(){ curl -s -o /dev/null -w "%{http_code}" "$@"; }
TBPAY='{"temperature":96}'
TRPAY='{"event":{"id":1,"type":"alarm","attributes":{"alarm":"sos"}},"device":{"uniqueId":"X-1"},"position":{"latitude":45,"longitude":9}}'

echo "== ingress rispetta i moduli spenti (globex: iot OFF) =="
C=$(code -X POST "$BASE/api/ingress/gps?token=globex-ingest-token" -H 'content-type: application/json' -d "$TRPAY")
[ "$C" = "403" ] && ok "ingress JSON dispositivi su modulo spento -> 403" || no "ingress gated" "$C"
# loneworker è attivo per globex: /api/events deve funzionare
C=$(code -X POST "$BASE/api/events" -H 'content-type: application/json' -H 'x-ingest-token: globex-ingest-token' \
  -d "{\"etype\":\"sos\",\"source_event\":\"mod-$(date +%s)\",\"correlation\":\"mod-$(date +%s)\"}")
[ "$C" = "200" ] && ok "ingress loneworker (modulo attivo) -> 200" || no "loneworker ingress" "$C"

echo "== superadmin accende 'iot' per globex SENZA deploy (ingress nativo) =="
S=$(mktemp)
curl -s -o /dev/null -c "$S" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$S" 2>/dev/null | tail -1)
GID=$(curl -s -b "$S" "$BASE/api/superadmin/tenants" | python -c "import sys,json;print([t['id'] for t in json.load(sys.stdin) if t['slug']=='globex'][0])")
sm(){ curl -s -o /dev/null -w "%{http_code}" -b "$S" -H 'content-type: application/json' -H "x-csrf-token: $CS" \
  -X POST "$BASE/api/superadmin/tenants/$GID/modules" -d "{\"module_code\":\"$1\",\"state\":\"$2\"}"; }
R=$(sm iot attivo); [ "$R" = "200" ] && ok "set_module iot=attivo (globex)" || no "enable" "$R"

# ora lo stesso ingress NON è più 403 (modulo attivo) -> crea l'incidente
C=$(code -X POST "$BASE/api/ingress/gps?token=globex-ingest-token" -H 'content-type: application/json' -d "$TRPAY")
[ "$C" != "403" ] && ok "dopo l'accensione l'ingress JSON dispositivi passa ($C, no deploy)" || no "post-enable" "$C"

echo "== rispegne 'iot' per globex =="
R=$(sm iot non_abilitato); [ "$R" = "200" ] && ok "set_module iot=non_abilitato" || no "disable" "$R"
C=$(code -X POST "$BASE/api/ingress/gps?token=globex-ingest-token" -H 'content-type: application/json' -d "$TRPAY")
[ "$C" = "403" ] && ok "ingress JSON dispositivi di nuovo bloccato -> 403" || no "re-gated" "$C"

echo "== isolamento: globex non vede un incidente specifico di acme (RLS) =="
ISO="iso-acme-$(date +%s)-$RANDOM"
curl -s -o /dev/null -X POST "$BASE/api/events" -H 'content-type: application/json' -H 'x-ingest-token: acme-ingest-token' \
  -d "{\"etype\":\"sos\",\"source_event\":\"$ISO\",\"idem_key\":\"$ISO\",\"correlation\":\"$ISO\"}"
G=$(mktemp)
curl -s -o /dev/null -c "$G" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: globex' \
  -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
curl -s -b "$G" -H 'x-tenant-slug: globex' "$BASE/api/incidents" | grep -q "$ISO" \
  && no "isolation" "globex vede $ISO di acme" || ok "globex non vede l'incidente acme $ISO (RLS)"

# teardown: chiude gli incidenti creati dalla suite (altrimenti restano aperti e le suite di
# escalation vedrebbero consegne estranee nel tenant)
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.incidents SET state='chiuso',closed_at=now(),outcome='e2e modularity'
  WHERE state<>'chiuso' AND (correlation LIKE 'mod-%' OR correlation LIKE 'iso-acme-%'
    OR (correlation='gps-evt-1' AND tenant_id=(SELECT id FROM app.tenants WHERE slug='globex')))" >/dev/null

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
