#!/usr/bin/env bash
# LONEWORKER — audit del cuore sicurezza:
#  F3) un check-in puo' essere confermato SOLO dal proprietario (no soppressione allarme altrui)
#  SOS worker -> incidente + richiesta notifica (timeline notify_requested/goalert_sent)
#  lifecycle incidente: ack (preso_in_carico) -> close (chiuso) + goalert_close
#  GoAlert piloted da YouWorkforce: gruppo+membri+routing -> sync -> goalert_routing popolato
#     (se GoAlert non e' provisionato per acme, la parte sync e' saltata e dichiarata)
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
echo "$TL" | grep -q 'goalert_sent' && ok "timeline: goalert_sent (inviato a GoAlert)" || no "manca goalert_sent" "$TL"

# ---- lifecycle: ack -> close ----
echo "$(oapi -X POST "$BASE/api/incidents/$IID/ack")" | grep -q '"ok":true' && ok "operatore ack incidente" || no "ack fallito" "-"
[ "$(sql "SELECT state FROM app.incidents WHERE id='$IID'")" = "preso_in_carico" ] && ok "stato -> preso_in_carico (human_ack)" || no "ack non ha cambiato stato" "-"
# un lavoratore NON puo' chiudere
echo "$(mapi -H 'content-type: application/json' -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"test"}')" | grep -qiE 'forbidden|cannot|role' && ok "lavoratore NON puo' chiudere incidenti" || no "ruolo non applicato su close" "-"
oapi -H 'content-type: application/json' -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"falso allarme (test e2e)"}' >/dev/null
[ "$(sql "SELECT state FROM app.incidents WHERE id='$IID'")" = "chiuso" ] && ok "operatore chiude con outcome -> chiuso" || no "close non ha chiuso" "-"
[ "$(sql "SELECT count(*) FROM app.outbox WHERE tenant_id=$TIDQ AND kind='goalert_close' AND payload->>'incident_id'='$IID'")" -ge 1 ] && ok "close -> goalert_close in outbox (close-loop)" || no "manca goalert_close" "-"

# ---- GoAlert piloted: gruppo -> EP/servizio/routing ----
GNAME="E2E Centrale $RANDOM"
aapi -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"$GNAME\"}" >/dev/null
GID=$(aapi "$BASE/api/groups" | python -c "import sys,json;d=json.load(sys.stdin);print([g['id'] for g in d['groups'] if g['name']=='$GNAME'][0])")
PUID=$(aapi "$BASE/api/users" | jv '[u["id"] for u in d if u["email"].startswith("operatore")][0]')
aapi -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$PUID\",\"position\":1}" >/dev/null
aapi -H 'content-type: application/json' -X POST "$BASE/api/routing" -d "{\"etype\":\"sos\",\"group_id\":\"$GID\"}" >/dev/null
SY=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/alarm/sync-goalert")
if echo "$SY" | grep -q '"ok":true'; then
  echo "$SY" | grep -q '"groups_synced"' && ok "sync GoAlert eseguito ($(echo "$SY" | jv 'd["groups_synced"]') gruppi, $(echo "$SY" | jv 'd["routes"]') routes)" || no "sync senza conteggi" "$SY"
  [ "$(sql "SELECT integration_key FROM app.goalert_routing WHERE tenant_id=$TIDQ AND etype='sos'")" = "$(sql "SELECT goalert_key FROM app.alarm_groups WHERE id='$GID'")" ] \
    && ok "routing sos -> integration key del gruppo (destinatari reali)" || no "goalert_routing non collegato al gruppo" "-"
  [ -n "$(sql "SELECT goalert_ep_id FROM app.alarm_groups WHERE id='$GID'")" ] && ok "gruppo -> escalation policy GoAlert creata" || no "EP non creata" "-"
else
  echo "SKIP  sync GoAlert non eseguibile (GoAlert non provisionato per acme): $(echo "$SY" | head -c 120)"
fi
# cleanup gruppo di test (lascia routing sos? no: ripristina)
aapi -H 'content-type: application/json' -X POST "$BASE/api/routing" -d '{"etype":"sos","group_id":null}' >/dev/null
sql "DELETE FROM app.alarm_group_members WHERE group_id='$GID'; DELETE FROM app.alarm_groups WHERE id='$GID'" >/dev/null
sql "DELETE FROM app.goalert_routing WHERE tenant_id=$TIDQ AND etype='sos'" >/dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
