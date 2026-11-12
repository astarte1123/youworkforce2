#!/usr/bin/env bash
# F3 residui — temporary schedule, multi-recapito+regole per-utente, dedup esplicito, label/ricerca.
# Tenant: globex (native). Stato ripristinato a fine run.
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
UID1=$(sql "SELECT user_id FROM app.alarm_group_members m JOIN app.tenants t ON t.id=m.tenant_id WHERE t.slug='$SLUG' AND m.group_id='$GID' LIMIT 1")
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
sql "UPDATE app.users SET phone='+390000000001' WHERE id='$UID1'" >/dev/null
# policy vuota + stato pulito
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[],"repeat":0}' >/dev/null
clean(){ sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null; sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null; }
mkinc(){ sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'$1',now()" >/dev/null; }
run(){ api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null; }

# ---- LABEL / ricerca ----
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/labels" -d '{"labels":{"env":"prod","team":"ops"}}' >/dev/null
LBL=$(api "$BASE/api/groups" | python -c "import sys,json;g=[x for x in json.load(sys.stdin)['groups'] if x['id']=='$GID'];print(g[0]['labels'].get('env','') if g else '')")
[ "$LBL" = prod ] && ok "label salvata e riletta" || no "label non riletta" "$LBL"
SR=$(api "$BASE/api/groups/search?key=env&value=prod" | python -c "import sys,json;print(len(json.load(sys.stdin)['groups']))")
[ "$SR" -ge 1 ] && ok "ricerca per label env=prod trova il gruppo" || no "ricerca label fallita" "$SR"
SR0=$(api "$BASE/api/groups/search?key=env&value=dev" | python -c "import sys,json;print(len(json.load(sys.stdin)['groups']))")
[ "$SR0" = 0 ] && ok "ricerca label inesistente = 0" || no "ricerca label falso positivo" "$SR0"

# ---- MULTI-RECAPITO + REGOLE per-utente: il runner usa il CM della regola ----
# CM voice su un numero distinto + regola immediata => consegna a quel numero via voice
CMID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/methods" -d '{"kind":"voice","value":"+390001112222","label":"cell"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
[ -n "$CMID" ] && ok "contact method aggiunto" || no "CM non creato" "-"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/rules" -d "{\"cm_id\":\"$CMID\",\"delay_min\":0}" >/dev/null
RL=$(api "$BASE/api/oncall-contacts/$UID1/methods" | python -c "import sys,json;print(len(json.load(sys.stdin)['rules']))")
[ "$RL" -ge 1 ] && ok "regola di notifica creata" || no "regola non creata" "$RL"
clean; mkinc "rest-rule-$RANDOM"; run
RCH=$(sql "SELECT channel||'|'||to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$RCH" = "voice|+390001112222" ] && ok "runner usa la regola per-utente (voice→CM)" || no "regola per-utente non applicata" "$RCH"
# rimuovi regola+CM => torna al recapito base (sms→phone)
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/methods/$CMID/noop" >/dev/null 2>&1 || true
RID=$(sql "SELECT id FROM app.user_notif_rules r JOIN app.tenants t ON t.id=r.tenant_id WHERE t.slug='$SLUG' LIMIT 1")
api -X DELETE "$BASE/api/oncall-contacts/rules/$RID" >/dev/null
api -X DELETE "$BASE/api/oncall-contacts/methods/$CMID" >/dev/null
clean; mkinc "rest-base-$RANDOM"; run
BCH=$(sql "SELECT channel||'|'||to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$BCH" = "sms|+390000000001" ] && ok "senza regole torna al recapito base (sms→phone)" || no "fallback base errato" "$BCH"

# ---- TEMPORARY SCHEDULE: sostituisce l'on-call ----
# crea un secondo utente nel tenant con numero distinto
U2=$(sql "SELECT u.id FROM app.users u JOIN app.tenants t ON t.id=u.tenant_id WHERE t.slug='$SLUG' AND u.id<>'$UID1' LIMIT 1")
if [ -z "$U2" ]; then U2=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'temp-$RANDOM@globex.loneworker.eu','x','operatore','+390009998888' FROM t RETURNING id"); else sql "UPDATE app.users SET phone='+390009998888' WHERE id='$U2'" >/dev/null; fi
NOWISO=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ENDISO=$(date -u -d '+1 day' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || python -c "import datetime;print((datetime.datetime.utcnow()+datetime.timedelta(days=1)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
TSID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules" -d "{\"group_id\":\"$GID\",\"start\":\"$NOWISO\",\"end\":\"$ENDISO\",\"blocks\":[{\"user_id\":\"$U2\",\"hours\":12}]}" | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
[ -n "$TSID" ] && ok "temporary schedule creata" || no "temp schedule non creata" "-"
clean; mkinc "rest-temp-$RANDOM"; run
TCH=$(sql "SELECT to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$TCH" = "+390009998888" ] && ok "temp schedule sostituisce l'on-call (consegna al blocco)" || no "temp schedule non applicata" "$TCH"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules/delete" -d "{\"id\":\"$TSID\"}" >/dev/null

# ---- DEDUP esplicito per correlation ----
CORR="dedup-fixed-key-$RANDOM"
sql "UPDATE app.incidents i SET state='chiuso' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.correlation='$CORR'" >/dev/null
# usa l'ingress generico con token traccar del tenant per inviare 2 critici stessa correlation
TOK=$(sql "SELECT it.token FROM app.ingest_tokens it JOIN app.tenants t ON t.id=it.tenant_id WHERE t.slug='$SLUG' AND it.source='traccar' LIMIT 1")
DEV=$(sql "SELECT d.external_id FROM app.devices d JOIN app.tenants t ON t.id=d.tenant_id WHERE t.slug='$SLUG' AND d.source='traccar' LIMIT 1")
if [ -n "$TOK" ] && [ -n "$DEV" ]; then
  curl -s -o /dev/null -X POST "$BASE/api/events" -H "x-ingest-token: $TOK" -H 'content-type: application/json' -d "{\"etype\":\"sos\",\"source_event\":\"$CORR-e1\",\"device_external_id\":\"$DEV\",\"correlation\":\"$CORR\"}"
  R2=$(curl -s -X POST "$BASE/api/events" -H "x-ingest-token: $TOK" -H 'content-type: application/json' -d "{\"etype\":\"sos\",\"source_event\":\"$CORR-e2\",\"device_external_id\":\"$DEV\",\"correlation\":\"$CORR\"}")
  DEDUP=$(echo "$R2" | python -c "import sys,json;print(json.load(sys.stdin).get('deduplicated',False))")
  OPEN=$(sql "SELECT count(*) FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation='$CORR' AND i.state<>'chiuso'")
  { [ "$DEDUP" = True ] && [ "$OPEN" = 1 ]; } && ok "dedup: 2° critico stessa correlation deduplicato (1 incidente)" || no "dedup non applicato" "dedup=$DEDUP open=$OPEN"
else no "dedup: setup token/device mancante" "-"; fi

# ---- teardown ----
sql "UPDATE app.alarm_groups SET labels='{}'::jsonb WHERE id='$GID'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e rest' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND (i.correlation LIKE 'rest-%' OR i.correlation='$CORR')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
