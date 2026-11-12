#!/usr/bin/env bash
# Parità pagina Services GoAlert (S1-S4):
#  S1 heartbeat monitor (recovery su ping; apertura via watchdog se silenzio oltre timeout)
#  S2 ingress email -> incidente instradato (+ dedup via +chiave)
#  S3 bulk ack-all / close-all
#  S4 on-call-now NATIVO (per gruppo, senza GoAlert)
# Tenant: globex (native). Stato ripristinato a fine run. NB: un test attende il watchdog (~30s).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
TID="(SELECT id FROM app.tenants WHERE slug='$SLUG')"
GID=$(api "$BASE/api/groups" | python -c "import sys,json;g=[x for x in json.load(sys.stdin)['groups'] if '$SLUG' in str(x['members'])];print(g[0]['id'] if g else '')")
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null

# ---- S4: on-call-now nativo ----
AV=$(api "$BASE/api/oncall-now-native" | python -c "import sys,json;d=json.load(sys.stdin);print(d.get('available'),d.get('engine'))")
[ "$AV" = "True native" ] && ok "S4 on-call-now nativo disponibile" || no "S4 on-call-now ko" "$AV"

# ---- S3: bulk ack-all / close-all ----
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','aperto',1,'svc-bulk-a-'||gen_random_uuid(),now()" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','aperto',1,'svc-bulk-b-'||gen_random_uuid(),now()" >/dev/null
ACK=$(api -X POST "$BASE/api/incidents/ack-all" | python -c "import sys,json;print(json.load(sys.stdin)['acknowledged'])")
[ "$ACK" -ge 2 ] && ok "S3 ack-all prende in carico ($ACK)" || no "S3 ack-all ko" "$ACK"
LEFT=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.state='aperto'")
[ "$LEFT" = 0 ] && ok "S3 nessun incidente resta 'aperto'" || no "S3 restano aperti" "$LEFT"
CL=$(api -H 'content-type: application/json' -X POST "$BASE/api/incidents/close-all" -d '{"outcome":"bulk e2e"}' | python -c "import sys,json;print(json.load(sys.stdin)['closed'])")
[ "$CL" -ge 2 ] && ok "S3 close-all chiude ($CL)" || no "S3 close-all ko" "$CL"
OPENN=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.state<>'chiuso'")
[ "$OPENN" = 0 ] && ok "S3 nessun incidente aperto dopo close-all" || no "S3 restano aperti dopo close" "$OPENN"

# ---- S2: ingress email -> incidente instradato ----
ADDR=$(api -H 'content-type: application/json' -X POST "$BASE/api/email-inboxes" -d "{\"group_id\":\"$GID\"}" >/dev/null; api "$BASE/api/email-inboxes" | python -c "import sys,json;b=json.load(sys.stdin)['inboxes'];print(b[-1]['address'] if b else '')")
[ -n "$ADDR" ] && ok "S2 indirizzo email generato" || no "S2 indirizzo ko" "-"
curl -s -o /dev/null -X POST "$BASE/api/ingress/email" -H 'content-type: application/json' -d "{\"to\":\"$ADDR\",\"from\":\"nagios@x\",\"subject\":\"Disco pieno\",\"body\":\"/ 98%\"}"
EM=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.etype='email_alarm' AND i.state<>'chiuso'")
[ "$EM" -ge 1 ] && ok "S2 email -> incidente email_alarm aperto" || no "S2 email non apre incidente" "$EM"
FG=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.etype='email_alarm' AND i.forced_group_id='$GID'")
[ "$FG" -ge 1 ] && ok "S2 incidente instradato al gruppo scelto (forced_group_id)" || no "S2 routing gruppo ko" "$FG"
# dedup via +chiave: 2 email stessa chiave -> 1 solo incidente
LOCAL="${ADDR%@*}"; DOM="${ADDR#*@}"; KEYADDR="${LOCAL}+disco@${DOM}"
curl -s -o /dev/null -X POST "$BASE/api/ingress/email" -H 'content-type: application/json' -d "{\"to\":\"$KEYADDR\",\"subject\":\"k1\"}"
curl -s -o /dev/null -X POST "$BASE/api/ingress/email" -H 'content-type: application/json' -d "{\"to\":\"$KEYADDR\",\"subject\":\"k2\"}"
DK=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation LIKE 'email-%+disco%' OR i.correlation LIKE 'email-%disco'")
# conta per correlation dedup-key stabile
DK=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation LIKE '%disco%' AND i.state<>'chiuso'")
[ "$DK" = 1 ] && ok "S2 dedup email per +chiave (1 incidente)" || no "S2 dedup email ko" "$DK"

# ---- S1: heartbeat monitor ----
HBID=$(api -H 'content-type: application/json' -X POST "$BASE/api/heartbeat-monitors" -d "{\"name\":\"nodo-critico\",\"timeout_min\":1,\"group_id\":\"$GID\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
URL=$(api "$BASE/api/heartbeat-monitors" | python -c "import sys,json;m=[x for x in json.load(sys.stdin)['monitors'] if x['id']=='$HBID'];print(m[0]['url'] if m else '')")
[ -n "$URL" ] && ok "S1 monitor creato con URL" || no "S1 monitor ko" "-"
# ping URL assoluto (in dev YW_ESC_PUBLIC_BASE puo' essere vuoto -> url relativo)
TOKEN="${URL##*/}"; PING="$BASE/api/ingress/heartbeat/$TOKEN"
# ping -> ok + last_ping
curl -s -o /dev/null -X POST "$PING"
ST=$(sql "SELECT state FROM app.heartbeat_monitors WHERE id='$HBID'")
[ "$ST" = ok ] && ok "S1 ping -> stato ok" || no "S1 ping ko" "$ST"
# recovery deterministico: forza down + incidente, poi ping chiude
sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT id,'heartbeat_down','aperto',2,'hb-rec-'||gen_random_uuid(),now() FROM t" >/dev/null
RECIID=$(sql "SELECT i.id FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation LIKE 'hb-rec-%' ORDER BY opened_at DESC LIMIT 1")
sql "UPDATE app.heartbeat_monitors SET state='down', incident_id='$RECIID' WHERE id='$HBID'" >/dev/null
curl -s -o /dev/null -X POST "$PING"
RECST=$(sql "SELECT state FROM app.incidents WHERE id='$RECIID'")
[ "$RECST" = chiuso ] && ok "S1 ping di recovery chiude l'incidente down" || no "S1 recovery ko" "$RECST"
# apertura via watchdog: silenzio oltre timeout -> incidente (attende il tick del worker)
sql "UPDATE app.heartbeat_monitors SET state='ok', incident_id=NULL, last_ping_at=now()-interval '5 min' WHERE id='$HBID'" >/dev/null
echo "   (attendo il watchdog ~35s)"; sleep 35
WST=$(sql "SELECT state FROM app.heartbeat_monitors WHERE id='$HBID'")
WINC=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.etype='heartbeat_down' AND i.state<>'chiuso' AND i.forced_group_id='$GID'")
{ [ "$WST" = down ] && [ "$WINC" -ge 1 ]; } && ok "S1 watchdog apre incidente heartbeat_down instradato" || no "S1 watchdog ko" "state=$WST inc=$WINC"

# ---- teardown ----
api -X DELETE "$BASE/api/heartbeat-monitors/$HBID" >/dev/null 2>&1 || true
sql "DELETE FROM app.email_inboxes e USING app.tenants t WHERE t.id=e.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e services' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND (i.correlation LIKE 'svc-%' OR i.correlation LIKE 'hb-%' OR i.etype IN ('email_alarm','heartbeat_down'))" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
