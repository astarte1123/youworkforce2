#!/usr/bin/env bash
# Parità pagina Profile GoAlert (G1-G4):
#  G1 opt-out/DNC (STOP via SMS inbound disabilita il recapito; il runner lo salta; START riabilita)
#  G2 verifica OTP su recapito aggiuntivo
#  G3 preferenza status-update per-utente (set/effettivo)
#  G4 note di passaggio turno + playbook del gruppo
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
PHONE="+390000777001"
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
sql "UPDATE app.users SET phone='$PHONE' WHERE id='$UID1'" >/dev/null
sql "DELETE FROM app.contact_optout WHERE value='$PHONE'" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[],"repeat":0}' >/dev/null
clean(){ sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null; sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null; }
mkinc(){ sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'$1',now()" >/dev/null; }
run(){ api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null; }

# ---- G1: opt-out via STOP ----
# consegna PRIMA dell'opt-out: il telefono riceve
clean; mkinc "prof-pre-$RANDOM"; run
PRE=$(sql "SELECT count(*) FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND el.to_value='$PHONE'")
[ "$PRE" -ge 1 ] && ok "pre opt-out: il recapito riceve" || no "pre opt-out niente consegna" "$PRE"
# STOP inbound
curl -s -o /dev/null -X POST "$BASE/api/ingress/twilio/sms" -H 'content-type: application/x-www-form-urlencoded' --data-urlencode "From=$PHONE" --data-urlencode "Body=STOP"
OPT=$(sql "SELECT count(*) FROM app.contact_optout WHERE value='$PHONE'")
POO=$(sql "SELECT phone_opt_out_at IS NOT NULL FROM app.users WHERE id='$UID1'")
{ [ "$OPT" = 1 ] && [ "$POO" = t ]; } && ok "STOP registra opt-out (DNC + phone_opt_out_at)" || no "STOP non registra opt-out" "opt=$OPT poo=$POO"
# consegna DOPO opt-out: saltato
clean; mkinc "prof-post-$RANDOM"; run
POST=$(sql "SELECT count(*) FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND el.to_value='$PHONE'")
[ "$POST" = 0 ] && ok "post opt-out: runner SALTA il recapito" || no "post opt-out ancora consegnato" "$POST"
# START riabilita
curl -s -o /dev/null -X POST "$BASE/api/ingress/twilio/sms" -H 'content-type: application/x-www-form-urlencoded' --data-urlencode "From=$PHONE" --data-urlencode "Body=START"
OPT2=$(sql "SELECT count(*) FROM app.contact_optout WHERE value='$PHONE'")
[ "$OPT2" = 0 ] && ok "START toglie dal DNC" || no "START non riabilita" "$OPT2"
clean; mkinc "prof-re-$RANDOM"; run
RE=$(sql "SELECT count(*) FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND el.to_value='$PHONE'")
[ "$RE" -ge 1 ] && ok "dopo START: il recapito riceve di nuovo" || no "dopo START niente consegna" "$RE"

# ---- G1 voce: tasto 9 = opt-out ----
PHONE2="+390000777009"
sql "DELETE FROM app.contact_optout WHERE value='$PHONE2'" >/dev/null
curl -s -o /dev/null -X POST "$BASE/api/ingress/twilio/voice" -H 'content-type: application/x-www-form-urlencoded' --data-urlencode "From=$PHONE2" --data-urlencode "Digits=9"
V=$(sql "SELECT count(*) FROM app.contact_optout WHERE value='$PHONE2'")
[ "$V" = 1 ] && ok "voce tasto 9 = opt-out" || no "voce opt-out fallito" "$V"
sql "DELETE FROM app.contact_optout WHERE value='$PHONE2'" >/dev/null

# ---- G2: verifica OTP su recapito aggiuntivo ----
CMID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/methods" -d '{"kind":"sms","value":"+390000888111","label":"cell2"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
api -X POST "$BASE/api/oncall-contacts/methods/$CMID/verify" >/dev/null
OTP=$(sql "SELECT verify_code FROM app.user_contact_methods WHERE id='$CMID'")
[ -n "$OTP" ] && ok "G2 OTP generato sul recapito aggiuntivo" || no "G2 OTP assente" "-"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/methods/$CMID/code" -d "{\"code\":\"$OTP\"}" >/dev/null
VER=$(sql "SELECT verified_at IS NOT NULL FROM app.user_contact_methods WHERE id='$CMID'")
[ "$VER" = t ] && ok "G2 recapito aggiuntivo verificato" || no "G2 non verificato" "$VER"

# ---- G3: preferenza status-update per-utente ----
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/$UID1/status-pref" -d '{"on":true}' >/dev/null
SP=$(sql "SELECT status_updates_on FROM app.users WHERE id='$UID1'")
[ "$SP" = t ] && ok "G3 status-pref per-utente salvata" || no "G3 status-pref non salvata" "$SP"
EFF=$(api "$BASE/api/oncall-contacts/$UID1/methods" | python -c "import sys,json;print(json.load(sys.stdin)['status_effective'])")
[ "$EFF" = True ] && ok "G3 status effettivo = on" || no "G3 effettivo errato" "$EFF"
# ack non deve dare errore (consegna status-update best-effort)
clean; mkinc "prof-ack-$RANDOM"; IID=$(sql "SELECT i.id FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id WHERE t.slug='$SLUG' AND i.correlation LIKE 'prof-ack-%' ORDER BY opened_at DESC LIMIT 1")
sql "UPDATE app.incidents SET state='aperto' WHERE id='$IID'" >/dev/null
AC=$(api -o /dev/null -w '%{http_code}' -X POST "$BASE/api/incidents/$IID/ack")
[ "$AC" = 200 ] && ok "G3 ack con status-update attivo: ok" || no "G3 ack fallito" "$AC"

# ---- G4: handoff note + playbook ----
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/playbook" -d '{"playbook":"1) verifica sede 2) chiama responsabile"}' >/dev/null
PB=$(api "$BASE/api/groups" | python -c "import sys,json;g=[x for x in json.load(sys.stdin)['groups'] if x['id']=='$GID'];print('ok' if g and 'responsabile' in g[0].get('playbook','') else 'no')")
[ "$PB" = ok ] && ok "G4 playbook salvato e letto" || no "G4 playbook ko" "$PB"
HID=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/handoff-notes" -d '{"body":"notte tranquilla, sensore A3 ballerino"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
HN=$(api "$BASE/api/groups/$GID/handoff-notes" | python -c "import sys,json;print(len(json.load(sys.stdin)['notes']))")
[ "$HN" -ge 1 ] && ok "G4 nota di passaggio creata e letta" || no "G4 nota ko" "$HN"
api -X DELETE "$BASE/api/handoff-notes/$HID" >/dev/null
HN2=$(api "$BASE/api/groups/$GID/handoff-notes" | python -c "import sys,json;print(len(json.load(sys.stdin)['notes']))")
[ "$HN2" -lt "$HN" ] && ok "G4 nota eliminata" || no "G4 nota non eliminata" "$HN2"

# ---- teardown ----
api -X DELETE "$BASE/api/oncall-contacts/methods/$CMID" >/dev/null 2>&1 || true
sql "DELETE FROM app.contact_optout WHERE value IN ('$PHONE','$PHONE2','+390000888111')" >/dev/null
sql "UPDATE app.users SET status_updates_on=NULL, phone_opt_out_at=NULL WHERE id='$UID1'" >/dev/null
sql "UPDATE app.alarm_groups SET playbook='' WHERE id='$GID'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e profile' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'prof-%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
