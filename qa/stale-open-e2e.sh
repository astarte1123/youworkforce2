#!/usr/bin/env bash
# CA-17 (mig 0141): ronde e presenze non restano aperte per sempre; una ronda in corso per giro.
#  - seconda ronda sullo stesso giro mentre una è in corso: 409 con chi l'ha avviata
#  - ronda dimenticata oltre finestra + tolleranza + 60 min: chiusa «abbandonata» con anomalia,
#    il giro torna avviabile; senza finestra il limite è 8 ore
#  - presenza di cantiere aperta oltre 16 ore: chiusa a durata zero, marcata da verificare
# Tenant usa-e-getta. Richiede TEST_CLOCK=1 (hook del controllo e orologio delle presenze).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
qq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
WD(){ curl -s -o /dev/null -X POST "$BASE/api/_test/stale-open-watchdog"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="so$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"So $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"giri\",\"cantieri\"]}" | py "d.get('id','')")
# l'orologio di test torna indietro a fine prova (gli altri test usano l'ora reale per le finestre)
trap 'curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H "content-type: application/json" -d "{\"seconds\":-57700}"; curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A POST /api/users "{\"email\":\"g2@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" >/dev/null
login G "g2@$SLUG.test" WorkPass9

echo "== una ronda in corso per giro =="
RT=$(call A POST /api/patrol-routes '{"name":"Perimetro","window_minutes":30}' | py "d['id']")
call A POST "/api/patrol-routes/$RT/checkpoints" '{"name":"Cancello","kind":"qr"}' >/dev/null
RD=$(call A POST "/api/patrol-routes/$RT/rounds" '{}' | py "d['id']")
[ -n "$RD" ] && ok "ronda avviata" || no "avvio" "-"
OUT=$(call G POST "/api/patrol-routes/$RT/rounds" '{}')
C=$(code G POST "/api/patrol-routes/$RT/rounds" '{}')
[ "$C" = "409" ] && echo "$OUT" | grep -q "a@$SLUG.test" && ok "seconda ronda sullo stesso giro: 409, dice chi l'ha avviata" || no "doppia" "$C $OUT"

echo "== ronda dimenticata =="
WD; [ "$(q "SELECT status FROM app.patrol_rounds WHERE id='$RD'")" = "in_corso" ] && ok "entro il limite resta in corso" || no "presto" "-"
qq "UPDATE app.patrol_rounds SET started_at=now() - interval '91 minutes' WHERE id='$RD'"
WD
[ "$(q "SELECT status||'/'||closed_by FROM app.patrol_rounds WHERE id='$RD'")" = "abbandonata/auto" ] && ok "oltre finestra+60 min: chiusa «abbandonata» dal sistema" || no "abbandono" "$(q "SELECT status FROM app.patrol_rounds WHERE id='$RD'")"
[ "$(q "SELECT count(*) FROM app.patrol_anomalies WHERE round_id='$RD' AND kind='abbandonata'")" = "1" ] && ok "anomalia «ronda abbandonata»" || no "anomalia" "-"
C=$(code G POST "/api/patrol-routes/$RT/rounds" '{}'); [ "$C" = "200" ] && ok "il giro torna avviabile" || no "riavvio" "$C"
RT2=$(call A POST /api/patrol-routes '{"name":"Interno"}' | py "d['id']")
RD2=$(call A POST "/api/patrol-routes/$RT2/rounds" '{}' | py "d['id']")
qq "UPDATE app.patrol_rounds SET started_at=now() - interval '7 hours' WHERE id='$RD2'"; WD
[ "$(q "SELECT status FROM app.patrol_rounds WHERE id='$RD2'")" = "in_corso" ] && ok "giro senza finestra: dopo 7 ore ancora in corso" || no "7h" "-"
qq "UPDATE app.patrol_rounds SET started_at=now() - interval '481 minutes' WHERE id='$RD2'"; WD
[ "$(q "SELECT status FROM app.patrol_rounds WHERE id='$RD2'")" = "abbandonata" ] && ok "giro senza finestra: oltre 8 ore chiusa" || no "8h" "-"

echo "== presenza dimenticata =="
CM=$(call A POST /api/commesse "{\"code\":\"S$R\",\"name\":\"Cantiere $R\"}" | py "d['id']")
call A POST "/api/commesse/$CM/presenze/in" '{}' >/dev/null
P=$(q "SELECT id FROM app.commessa_presenze WHERE commessa_id='$CM'")
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":54000}'; WD
[ "$(q "SELECT clock_out IS NULL FROM app.commessa_presenze WHERE id='$P'")" = "t" ] && ok "dopo 15 ore la presenza resta aperta" || no "15h" "-"
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":3700}'; WD
[ "$(q "SELECT (clock_out=clock_in)::text||'/'||auto_closed FROM app.commessa_presenze WHERE id='$P'")" = "true/true" ] \
  && ok "oltre 16 ore: chiusa a durata zero e marcata da verificare" || no "16h" "$(q "SELECT clock_out, auto_closed FROM app.commessa_presenze WHERE id='$P'")"
call A GET "/api/commesse/$CM" | py "d['presenze'][0]['auto_closed'] and d['cost']['labor_hours']==0" | grep -q True \
  && ok "nessuna ora conteggiata; la scheda lo segnala" || no "scheda" "-"

echo "stale-open: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
