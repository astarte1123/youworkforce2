#!/usr/bin/env bash
# LONEWORKER — audit del cuore sicurezza:
#  F3) un check-in puo' essere confermato SOLO dal proprietario (no soppressione allarme altrui)
#  SOS worker -> incidente + richiesta notifica (timeline notify_requested/notify_dispatched nativo)
#  lifecycle incidente: ack (preso_in_carico) -> close (chiuso) + outbox kind='close' (close-loop nativo)
#  YWAlert piloted da YouWorkforce: gruppo+membri+routing -> sync -> goalert_routing popolato
#     (se YWAlert non e' provisionato per acme, la parte sync e' saltata e dichiarata)
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

login(){ local jar=$1 email=$2; curl -s -o /dev/null -c "$jar" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$email\",\"password\":\"demo\"}"; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }

MARIO=$(mktemp); login "$MARIO" "mario@acme.loneworker.eu"; MC=$(csrf "$MARIO")
mapi(){ curl -s -b "$MARIO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $MC" "$@"; }
OP=$(mktemp); login "$OP" "operatore@acme.loneworker.eu"; OC=$(csrf "$OP")
oapi(){ curl -s -b "$OP" -H 'x-tenant-slug: acme' -H "x-csrf-token: $OC" "$@"; }
AD=$(mktemp); login "$AD" "admin@acme.loneworker.eu"; AC=$(csrf "$AD")
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $AC" "$@"; }

# ---- F3: ownership del check-in ----
CID=$(mapi -H 'content-type: application/json' -X POST "$BASE/api/checkins" -d '{"minutes":30}' | jv 'd["id"]')
[ -n "$CID" ] && ok "check-in creato dal lavoratore" || no "checkin non creato" "-"
# operatore (NON proprietario) prova a confermare -> deve fallire (ok:false)
R=$(oapi -X POST "$BASE/api/checkins/$CID/confirm")
echo "$R" | grep -q '"ok":false' && ok "F3: altro utente NON puo' confermare il check-in altrui" || no "F3 ownership bucata" "$R"
[ "$(sql "SELECT state FROM app.checkins WHERE id='$CID'")" = "attivo" ] && ok "check-in resta attivo dopo tentativo non autorizzato" || no "stato checkin alterato" "-"
# proprietario conferma -> ok
R=$(mapi -X POST "$BASE/api/checkins/$CID/confirm")
echo "$R" | grep -q '"ok":true' && ok "proprietario conferma il proprio check-in" || no "owner non puo' confermare" "$R"
[ "$(sql "SELECT state FROM app.checkins WHERE id='$CID'")" = "confermato" ] && ok "check-in confermato" || no "stato non confermato" "-"

# ---- SOS worker -> incidente + notifica ----
SOS=$(mapi -H 'content-type: application/json' -X POST "$BASE/api/sos" -d '{"lat":45.46,"lon":9.19}')
IID=$(echo "$SOS" | jv 'd["incident_id"]')
[ -n "$IID" ] && ok "SOS -> incidente aperto" || no "SOS non ha aperto incidente" "$SOS"
[ "$(sql "SELECT etype FROM app.incidents WHERE id='$IID'")" = "sos" ] && ok "incidente etype=sos" || no "etype errato" "-"
[ "$(sql "SELECT state FROM app.incidents WHERE id='$IID'")" = "aperto" ] && ok "incidente stato=aperto" || no "stato iniziale errato" "-"
sleep 2
TL=$(oapi "$BASE/api/incidents/$IID/timeline")
echo "$TL" | grep -q 'notify_requested' && ok "timeline: notify_requested" || no "manca notify_requested" "$TL"
echo "$TL" | grep -q 'notify_dispatched' && ok "timeline: notify_dispatched (consegna nativa)" || no "manca notify_dispatched" "$TL"

# ---- lifecycle: ack -> close ----
echo "$(oapi -X POST "$BASE/api/incidents/$IID/ack")" | grep -q '"ok":true' && ok "operatore ack incidente" || no "ack fallito" "-"
[ "$(sql "SELECT state FROM app.incidents WHERE id='$IID'")" = "preso_in_carico" ] && ok "stato -> preso_in_carico (human_ack)" || no "ack non ha cambiato stato" "-"
# un lavoratore NON puo' chiudere
echo "$(mapi -H 'content-type: application/json' -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"test"}')" | grep -qiE 'forbidden|cannot|role' && ok "lavoratore NON puo' chiudere incidenti" || no "ruolo non applicato su close" "-"
oapi -H 'content-type: application/json' -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"falso allarme (test e2e)"}' >/dev/null
[ "$(sql "SELECT state FROM app.incidents WHERE id='$IID'")" = "chiuso" ] && ok "operatore chiude con outcome -> chiuso" || no "close non ha chiuso" "-"
[ "$(sql "SELECT count(*) FROM app.outbox WHERE tenant_id=$TIDQ AND kind='close' AND payload->>'incident_id'='$IID'")" -ge 1 ] && ok "close -> kind='close' in outbox (close-loop nativo)" || no "manca close" "-"

# NB: la vecchia sezione "provisioning YWAlert esterno" (sync-goalert -> EP/servizio/routing/rotation
# su GoAlert) è stata RIMOSSA: l'escalation è nativa (nessun provisioning esterno). La parità del
# motore nativo (rotation/turni/policy/override/consegna reale) è coperta dalle suite escalation-*
# e ywalert-delivery-e2e.

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
