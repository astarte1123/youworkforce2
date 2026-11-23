#!/usr/bin/env bash
# F3 residui — temporary schedule, multi-recapito+regole per-utente, dedup esplicito, label/ricerca.
# Tenant: globex (native). ISOLATO: gruppo/utenti/token/device dedicati, forced_group sugli incidenti,
# teardown completo -> idempotente, nessun residuo, immune a routing/gruppi lasciati da altre suite.
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

R=$RANDOM
# gruppo + utenti dedicati (telefoni UNICI range 3966), mai riuso di admin/LIMIT 1
GID=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Rest-$R\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
UID1=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'rest1-$R@globex.loneworker.eu','x','operatore','+393966000001' FROM t RETURNING id" | head -1)
U2=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'rest2-$R@globex.loneworker.eu','x','operatore','+393966000002' FROM t RETURNING id" | head -1)
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$UID1\",\"position\":1}" >/dev/null
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id) SELECT $TID,'$GID' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID'" >/dev/null
# policy vuota + stato pulito
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[],"repeat":0}' >/dev/null
clean(){ sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null; sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null; }
# forced_group_id: pinna gli incidenti al gruppo di test (immune a routing/gruppi residui)
mkinc(){ sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) SELECT $TID,'sos','nuovo',1,'$1',now(),'$GID'" >/dev/null; }
run(){ api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null; }

# ---- LABEL / ricerca ----
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/labels" -d '{"labels":{"env":"prod","team":"ops"}}' >/dev/null
LBL=$(api "$BASE/api/groups" | python -c "import sys,json;g=[x for x in json.load(sys.stdin)['groups'] if x['id']=='$GID'];print(g[0]['labels'].get('env','') if g else '')")
[ "$LBL" = prod ] && ok "label salvata e riletta" || no "label non riletta" "$LBL"
SR=$(api "$BASE/api/groups/search?key=env&value=prod" | python -c "import sys,json;print(len([x for x in json.load(sys.stdin)['groups'] if x['id']=='$GID']))")
[ "$SR" -ge 1 ] && ok "ricerca per label env=prod trova il gruppo" || no "ricerca label fallita" "$SR"
SR0=$(api "$BASE/api/groups/search?key=env&value=dev-$R" | python -c "import sys,json;print(len(json.load(sys.stdin)['groups']))")
[ "$SR0" = 0 ] && ok "ricerca label inesistente = 0" || no "ricerca label falso positivo" "$SR0"

# ---- MULTI-RECAPITO + REGOLE per-utente: il runner usa il CM della regola ----
# CM voice su un numero distinto + regola immediata => consegna a quel numero via voice
CMID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/methods" -d '{"kind":"voice","value":"+393966001122","label":"cell"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
[ -n "$CMID" ] && ok "contact method aggiunto" || no "CM non creato" "-"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/rules" -d "{\"cm_id\":\"$CMID\",\"delay_min\":0}" >/dev/null
RL=$(api "$BASE/api/oncall-contacts/$UID1/methods" | python -c "import sys,json;print(len(json.load(sys.stdin)['rules']))")
[ "$RL" -ge 1 ] && ok "regola di notifica creata" || no "regola non creata" "$RL"
clean; mkinc "rest-rule-$R"; run
RCH=$(sql "SELECT channel||'|'||to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$RCH" = "voice|+393966001122" ] && ok "runner usa la regola per-utente (voice→CM)" || no "regola per-utente non applicata" "$RCH"
# rimuovi regola+CM => torna al recapito base (sms→phone)
RID=$(sql "SELECT r.id FROM app.user_notif_rules r WHERE r.user_id='$UID1' LIMIT 1")
api -X DELETE "$BASE/api/oncall-contacts/rules/$RID" >/dev/null
api -X DELETE "$BASE/api/oncall-contacts/methods/$CMID" >/dev/null
clean; mkinc "rest-base-$R"; run
BCH=$(sql "SELECT channel||'|'||to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$BCH" = "sms|+393966000001" ] && ok "senza regole torna al recapito base (sms→phone)" || no "fallback base errato" "$BCH"

# ---- TEMPORARY SCHEDULE: sostituisce l'on-call ----
NOWISO=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ENDISO=$(date -u -d '+1 day' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || python -c "import datetime;print((datetime.datetime.utcnow()+datetime.timedelta(days=1)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TSID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules" -d "{\"group_id\":\"$GID\",\"start\":\"$NOWISO\",\"end\":\"$ENDISO\",\"blocks\":[{\"user_id\":\"$U2\",\"hours\":12}],\"allow_gaps\":true}" | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
[ -n "$TSID" ] && ok "temporary schedule creata" || no "temp schedule non creata" "-"
clean; mkinc "rest-temp-$R"; run
TCH=$(sql "SELECT to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$TCH" = "+393966000002" ] && ok "temp schedule sostituisce l'on-call (consegna al blocco)" || no "temp schedule non applicata" "$TCH"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules/delete" -d "{\"id\":\"$TSID\"}" >/dev/null

# ---- DEDUP esplicito per correlation ----
# I token di ingest sono HASHATI (la colonna token e' un handle non-segreto): non si possono
# rigiocare dal DB. Provisioniamo un token LEGACY plaintext (token_hash NULL) + un device dedicati.
CORR="dedup-$R"
TOK="e2e-rest-tok-$R"
DEV="E2E-REST-$R"
sql "INSERT INTO app.devices(tenant_id,external_id,source,label) SELECT $TID,'$DEV','gps','e2e rest dedup'" >/dev/null
sql "INSERT INTO app.ingest_tokens(tenant_id,source,token,token_hash) SELECT $TID,'gps','$TOK',NULL" >/dev/null
curl -s -o /dev/null -X POST "$BASE/api/events" -H "x-ingest-token: $TOK" -H 'content-type: application/json' -d "{\"etype\":\"sos\",\"source_event\":\"$CORR-e1\",\"device_external_id\":\"$DEV\",\"correlation\":\"$CORR\"}"
R2=$(curl -s -X POST "$BASE/api/events" -H "x-ingest-token: $TOK" -H 'content-type: application/json' -d "{\"etype\":\"sos\",\"source_event\":\"$CORR-e2\",\"device_external_id\":\"$DEV\",\"correlation\":\"$CORR\"}")
DEDUP=$(echo "$R2" | python -c "import sys,json;print(json.load(sys.stdin).get('deduplicated',False))")
OPEN=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation='$CORR' AND i.state<>'chiuso'")
{ [ "$DEDUP" = True ] && [ "$OPEN" = 1 ]; } && ok "dedup: 2° critico stessa correlation deduplicato (1 incidente)" || no "dedup non applicato" "dedup=$DEDUP open=$OPEN"

# ---- teardown COMPLETO ----
api -X DELETE "$BASE/api/groups/$GID" >/dev/null 2>&1 || true
sql "DELETE FROM app.alarm_group_members WHERE group_id='$GID'" >/dev/null
clean
sql "DELETE FROM app.tenant_oncall_cfg oc USING app.tenants t WHERE t.id=oc.tenant_id AND t.slug='$SLUG' AND oc.default_group_id='$GID'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e rest' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND (i.correlation LIKE 'rest-%' OR i.correlation='$CORR')" >/dev/null
# rimuovi device+token dedicati e gli eventi/telemetria collegati (device eliminabile del tutto, mig 0109)
sql "DELETE FROM app.ingest_tokens it USING app.tenants t WHERE t.id=it.tenant_id AND t.slug='$SLUG' AND it.token='$TOK'" >/dev/null
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id='$DEV'" >/dev/null
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'rest1-%@globex%'" >/dev/null
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'rest2-%@globex%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
