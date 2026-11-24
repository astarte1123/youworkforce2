#!/usr/bin/env bash
# SA-1 + SA-2 (mig 0129): il modulo GPS è attivabile dal superadmin e funziona DA SOLO.
#  - SA-1: prima della 0129 `gps` ereditava available=false dal vecchio connettore traccar e
#          set_module rispondeva 400 "modulo ritirato" su qualunque tenant.
#  - SA-2: un tenant con solo GPS (niente Lone Worker né IoT) vede la Centrale allarmi e configura
#          l'escalation; i check-in restano di Lone Worker (403).
# Usa globex; salva e ripristina lo stato dei moduli. Mai acme.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }

A=$(q "SELECT available FROM app.modules_catalog WHERE code='gps'")
[ "$A" = "t" ] && ok "catalogo: gps disponibile (0129)" || no "catalogo gps" "available=$A"

S=$(mktemp); G=$(mktemp)
curl -s -o /dev/null -c "$S" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$S" 2>/dev/null | tail -1)
GID=$(curl -s -b "$S" "$BASE/api/superadmin/tenants" | python -c "import sys,json;print([t['id'] for t in json.load(sys.stdin) if t['slug']=='globex'][0])")
sm(){ curl -s -o /dev/null -w "%{http_code}" -b "$S" -H 'content-type: application/json' -H "x-csrf-token: $CS" \
  -X POST "$BASE/api/superadmin/tenants/$GID/modules" -d "{\"module_code\":\"$1\",\"state\":\"$2\"}"; }
st(){ q "SELECT COALESCE((SELECT state FROM app.tenant_modules WHERE tenant_id='$GID' AND module_code='$1'),'non_abilitato')"; }
PREV_LW=$(st loneworker); PREV_IOT=$(st iot); PREV_GPS=$(st gps); PREV_GIRI=$(st giri)
restore(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "
  UPDATE app.tenant_modules SET state='$PREV_LW'  WHERE tenant_id='$GID' AND module_code='loneworker';
  UPDATE app.tenant_modules SET state='$PREV_IOT' WHERE tenant_id='$GID' AND module_code='iot';
  UPDATE app.tenant_modules SET state='$PREV_GPS' WHERE tenant_id='$GID' AND module_code='gps';
  UPDATE app.tenant_modules SET state='$PREV_GIRI' WHERE tenant_id='$GID' AND module_code='giri';" >/dev/null; }
trap restore EXIT

echo "== SA-1: il superadmin attiva gps =="
R=$(sm gps non_abilitato); [ "$R" = "200" ] || no "reset gps" "$R"
R=$(sm gps attivo); [ "$R" = "200" ] && ok "set_module gps=attivo -> 200" || no "attiva gps" "$R"
[ "$(st gps)" = "attivo" ] && ok "tenant_modules: gps attivo" || no "stato gps" "$(st gps)"

echo "== SA-2: globex con SOLO gps =="
# precondizione via SQL (spegnere loneworker via API è bloccato se ci sono incidenti aperti)
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$GID' AND module_code IN ('loneworker','iot')" >/dev/null
curl -s -o /dev/null -c "$G" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: globex' \
  -d '{"email":"admin@globex.loneworker.eu","password":"demo"}'
g(){ curl -s -o /dev/null -w "%{http_code}" -b "$G" -H 'x-tenant-slug: globex' "$BASE$1"; }
C=$(g /api/incidents);     [ "$C" = "200" ] && ok "Centrale allarmi (/api/incidents) con solo gps -> 200" || no "incidents" "$C"
C=$(g /api/groups);        [ "$C" = "200" ] && ok "escalation (/api/groups) con solo gps -> 200" || no "groups" "$C"
C=$(g /api/checkins/list); [ "$C" = "403" ] && ok "check-in restano di Lone Worker -> 403" || no "checkins" "$C"

echo "== nessun modulo che produce allarmi -> Centrale vietata =="
# anche giri: dal 24/11/2026 (CA-12) il modulo Ronde produce l'allarme «Ronda mancata»
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$GID' AND module_code IN ('gps','giri')" >/dev/null
C=$(g /api/incidents); [ "$C" = "403" ] && ok "senza loneworker/gps/iot/giri -> 403" || no "incidents gated" "$C"

rm -f "$S" "$G"
echo "gps-standalone: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
