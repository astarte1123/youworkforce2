#!/usr/bin/env bash
# Verifica di UPGRADE della migrazione 0127 partendo da un DB con 0125+0126 già applicate.
#
# Non fa parte di run-all.sh: richiede un dump a versione 126 (DUMP, dentro il container ywdb) e il
# binario del core NUOVO (BIN). Crea un DB usa-e-getta, vi inserisce i casi limite, avvia il core su
# porte separate (il core applica 0127 PRIMA di avviare worker/bridge) e controlla:
#   - dopo la migrazione, prima dei worker: armamento watchdog, identità GPS, traccar_conn_down,
#     incidenti perdita_contatto preesistenti, stato escalation migrato (casi a/b/c);
#   - dopo i primi giri dei worker: nessun falso perdita_contatto, scala ripartita solo dove nessuno
#     era stato avvisato, nessun doppione dove la consegna era reale;
#   - bridge Teltonika simulato: IMEI conteso tra tenant rifiutato.
# Uso: DUMP=/tmp/yw_pre_0127.dump BIN=core/target/debug/yw-core.exe bash qa/upgrade-0127-check.sh
set -uo pipefail
export MSYS_NO_PATHCONV=1   # Git Bash: non convertire i percorsi interni al container
DUMP="${DUMP:-/tmp/yw_pre_0127.dump}"; BIN="${BIN:-core/target/debug/yw-core.exe}"
DB=yw_upgrade_0127; HP=18127; TP=15127; MP=11127
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d "$DB" -v ON_ERROR_STOP=1 -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }

docker exec ywdb psql -U postgres -tAc "DROP DATABASE IF EXISTS $DB" >/dev/null
docker exec ywdb psql -U postgres -tAc "CREATE DATABASE $DB" >/dev/null
docker exec ywdb psql -U postgres -tAc "ALTER DATABASE $DB SET search_path=app, public" >/dev/null
docker exec ywdb pg_restore -U postgres -d "$DB" --no-owner "$DUMP" 2>/dev/null
V=$(one "SELECT max(version) FROM _sqlx_migrations")
[ "$V" = "126" ] && ok "DB di partenza a versione 126 (0125+0126 applicate)" || { no "versione di partenza" "$V"; exit 1; }

ACME=$(one "SELECT id FROM app.tenants WHERE slug='acme'"); GLOBEX=$(one "SELECT id FROM app.tenants WHERE slug='globex'")
GRP=$(one "SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id='$ACME'")
WORKER=$(one "SELECT id FROM app.users WHERE tenant_id='$ACME' AND role='lavoratore' LIMIT 1")
ONCALL=$(one "SELECT id FROM app.users WHERE tenant_id='$ACME' AND role='responsabile' LIMIT 1")
PHONE="+390000000127"
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$ACME','gps','attivo'),('$ACME','loneworker','attivo'),('$GLOBEX','gps','attivo')
     ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
# destinatari deterministici: scala esplicita del gruppo di default verso un solo utente
sql "UPDATE app.users SET phone='$PHONE' WHERE id='$ONCALL'; DELETE FROM app.user_notif_rules WHERE user_id='$ONCALL';
     DELETE FROM app.esc_policy_steps WHERE group_id='$GRP';
     INSERT INTO app.esc_policy_steps(tenant_id,group_id,idx,delay_s,channel,target_kind,target_user_id)
       VALUES('$ACME','$GRP',0,300,'sms','user','$ONCALL'),('$ACME','$GRP',1,0,'voice','user','$ONCALL');
     DELETE FROM app.esc_delivery_log WHERE tenant_id='$ACME'; DELETE FROM app.esc_alert_state WHERE tenant_id='$ACME';
     UPDATE app.incidents SET state='chiuso', closed_at=now() WHERE tenant_id='$ACME' AND state<>'chiuso'" >/dev/null

# --- casi limite (stato "come dopo 0125/0126", prima di 0127) ---
# badge assegnato, ultimo contatto/posizione 3 ore fa (seminato da 0125 con last_pos_at)
D_OLD=$(one "INSERT INTO app.devices(tenant_id,external_id,source,label,assigned_user,enabled,last_pos_at,last_seen_at,last_lat,last_lon)
             VALUES('$ACME','UPG-OLD-1','gps','badge cassetto','$WORKER',true,now()-interval '3 hours',now()-interval '3 hours',45.1,9.1) RETURNING id")
# IMEI conteso tra due tenant (duplicato preesistente)
DUP=356000000000127
sql "INSERT INTO app.devices(tenant_id,external_id,source,label,enabled) VALUES('$ACME','$DUP','gps','dup A',true),('$GLOBEX','$DUP','gps','dup B',true)" >/dev/null
# incidenti aperti: traccar_conn_down (connettore ritirato) e perdita_contatto (origine non verificabile)
I_TCD=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation) VALUES('$ACME','traccar_conn_down','aperto',1,'upg-tcd') RETURNING id")
I_PC=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation) VALUES('$ACME','perdita_contatto','aperto',1,'upg-pc') RETURNING id")
sql "INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES('$ACME','$I_TCD',1,now()-interval '1 hour','unack')" >/dev/null
sql "INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES('$ACME','$I_PC',1,now()-interval '1 hour','exhausted')" >/dev/null
# (a) solo in ombra: gradino 0 "consegnato" al sink, nessuno avvisato davvero
I_A=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,forced_group_id) VALUES('$ACME','sos','aperto',1,'upg-a','$GRP') RETURNING id")
# (b) consegna reale (live) del gradino 0
I_B=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,forced_group_id) VALUES('$ACME','sos','aperto',1,'upg-b','$GRP') RETURNING id")
# (c) accettato da GoAlert, nessuno stato nativo
I_C=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,forced_group_id) VALUES('$ACME','sos','aperto',1,'upg-c','$GRP') RETURNING id")
for X in "$I_A:false" "$I_B:true"; do
  I=${X%%:*}; L=${X##*:}
  sql "INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES('$ACME','$I',0,now()-interval '1 minute','unack');
       INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES('$ACME','$I',0,'sms','$PHONE',0,'sent');
       INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES('$ACME','$I','esc_delivery','{\"step\":0,\"channel\":\"sms\",\"engine\":\"goalert\",\"live\":$L}')" >/dev/null
done
sql "INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES('$ACME','$I_C','notify_dispatched','{\"alert_id\":77}')" >/dev/null

# (d) già preso in carico: non toccato
I_D=$(one "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,forced_group_id) VALUES('$ACME','sos','preso_in_carico',1,'upg-d','$GRP') RETURNING id")

# --- controllo IN SOLA LETTURA prima della migrazione: esito previsto per incidente ---
CHK=$(docker exec -i ywdb psql -U postgres -d "$DB" -tA -F '|' -v ON_ERROR_STOP=1 < "$(dirname "$0")/sql/check-escalation-0127.sql")
esito(){ echo "$CHK" | awk -F'|' -v id="$1" '$2==id{print $12}'; }
[ "$(esito "$I_TCD")" = "chiusura" ] && [ "$(esito "$I_A")" = "riavvio_primo_gradino" ] && [ "$(esito "$I_B")" = "continuita" ]   && [ "$(esito "$I_C")" = "possibile_doppio_avviso" ] && [ "$(esito "$I_D")" = "invariato_preso_in_carico" ] && [ "$(esito "$I_PC")" = "continuita" ]   && ok "controllo in sola lettura: esito previsto corretto per ogni incidente"   || no "check escalation" "tcd=$(esito "$I_TCD") a=$(esito "$I_A") b=$(esito "$I_B") c=$(esito "$I_C") d=$(esito "$I_D") pc=$(esito "$I_PC")"
[ "$(one "SELECT max(version) FROM _sqlx_migrations")" = "126" ] && ok "il controllo non ha scritto nulla (DB ancora a 126)" || no "check read-only"
# IMEI contesi (sola lettura, schema pre-0128): il duplicato compare con i due tenant
IMC=$(docker exec -i ywdb psql -U postgres -d "$DB" -tA -F '|' -v ON_ERROR_STOP=1 < "$(dirname "$0")/sql/check-imei-contesi.sql")
[ "$(echo "$IMC" | awk -F'|' -v i="$DUP" '$1==i' | wc -l)" = "2" ] && echo "$IMC" | grep -q "^1|"   && ok "controllo IMEI contesi: duplicato elencato per entrambi i tenant (schema pre-0128)" || no "check imei" "$IMC"

# --- avvio core nuovo: applica 0127, poi parte il worker ---
SINK_LOG="${YW_ESC_SINK_LOG:?imposta YW_ESC_SINK_LOG al log del ricevitore delle notifiche simulate}"
SINK0=$(wc -c < "$SINK_LOG")
LOG=$(mktemp)
T0=$(one "SELECT to_char(now() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
YW_DB_OWNER="postgres://postgres:postgres@localhost:55432/$DB" YW_DB_RT="postgres://app_rt:app_rt_pw@localhost:55432/$DB" \
YW_SECRET_KEY="${YW_SECRET_KEY:-dev-secret-key-0123456789abcdef0123456789abcdef}" YW_BIND="127.0.0.1:$HP" YW_TELTONIKA_PORT=$TP YW_MQTT_PORT=$MP \
YW_ESC_SINK_URL="http://127.0.0.1:18099/" "$BIN" >"$LOG" 2>&1 &
CPID=$!
# stato SUBITO dopo la migrazione: si attende solo che 0127 risulti applicata
for i in $(seq 1 120); do [ "$(one "SELECT count(*) FROM _sqlx_migrations WHERE version=127" 2>/dev/null)" = "1" ] && break; sleep 0.25; done
[ "$(one "SELECT count(*) FROM _sqlx_migrations WHERE version=127")" = "1" ] && ok "0127 applicata all'avvio" || no "0127 non applicata" "$(tail -5 "$LOG")"

[ "$(one "SELECT silence_armed_at IS NOT NULL AND silence_armed_at >= '$T0'::timestamptz FROM app.devices WHERE id='$D_OLD'")" = "t" ] \
  && ok "watchdog armato dal momento dell'upgrade (badge esistente)" || no "armamento" "-"
[ "$(one "SELECT state FROM app.incidents WHERE id='$I_TCD'")" = "chiuso" ] && ok "traccar_conn_down aperto -> chiuso" || no "tcd" "$(one "SELECT state FROM app.incidents WHERE id='$I_TCD'")"
[ "$(one "SELECT status FROM app.esc_alert_state WHERE incident_id='$I_TCD'")" = "closed" ] && ok "traccar_conn_down: escalation ferma (stato closed)" || no "tcd esc" "-"
[ "$(one "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$I_TCD' AND detail->>'reason'='connettore_ritirato'")" = "1" ] && ok "traccar_conn_down: chiusura tracciata in timeline" || no "tcd timeline" "-"
[ "$(one "SELECT state FROM app.incidents WHERE id='$I_PC'")" = "aperto" ] && ok "perdita_contatto preesistente lasciato aperto" || no "pc" "-"
[ "$(one "SELECT count(*) FROM app.esc_alert_state WHERE incident_id='$I_A'")" = "0" ] && [ "$(one "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$I_A' AND kind='escalation_migrated' AND detail->>'action'='restart' AND jsonb_array_length(detail->'shadow_deliveries')=1")" = "1" ] \
  && ok "(a) solo ombra: scala azzerata, consegne in ombra archiviate in timeline" || no "(a)" "-"
[ "$(one "SELECT step||':'||status FROM app.esc_alert_state WHERE incident_id='$I_B'")" = "0:unack" ] && [ "$(one "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$I_B'")" = "1" ] \
  && ok "(b) consegna reale: stato e log intatti" || no "(b)" "-"
[ "$(one "SELECT count(*) FROM app.incident_timeline WHERE incident_id='$I_C' AND kind='escalation_migrated' AND detail->>'action'='review'")" = "1" ] \
  && ok "(c) accettato da GoAlert: annotato per revisione" || no "(c)" "-"
# identità GPS: un NUOVO duplicato tra tenant è rifiutato, la modifica di un duplicato vecchio no
E=$(docker exec -i ywdb psql -U postgres -d "$DB" -tAc "INSERT INTO app.devices(tenant_id,external_id,source) VALUES('$(one "SELECT id FROM app.tenants WHERE slug='turni'")','$DUP','gps')" 2>&1)
echo "$E" | grep -q "già registrato" && ok "nuovo IMEI duplicato tra tenant rifiutato dal DB" || no "trigger identità" "$E"
E=$(docker exec -i ywdb psql -U postgres -d "$DB" -tAc "INSERT INTO app.devices(tenant_id,external_id,source) VALUES('$GLOBEX','UPG-OLD-1','gps')" 2>&1)
echo "$E" | grep -q "già registrato" && ok "IMEI di un altro tenant non registrabile" || no "trigger identità 2" "$E"
sql "UPDATE app.devices SET label='dup A rinominato' WHERE tenant_id='$ACME' AND external_id='$DUP'" >/dev/null 2>&1 \
  && ok "duplicato preesistente ancora modificabile (etichetta)" || no "update duplicato" "-"

# --- primi giri dei worker (watchdog ~10s, escalation ~30s) ---
for i in $(seq 1 120); do curl -s -o /dev/null "http://127.0.0.1:$HP/health" && break; sleep 0.5; done
sleep 40
N=$(one "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$D_OLD' AND i.etype='perdita_contatto'")
[ "$N" = "0" ] && ok "nessun falso perdita_contatto al primo avvio (badge silente da 3h)" || no "watchdog al primo avvio" "$N incidenti"
NA=$(one "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$I_A' AND step=0 AND at >= '$T0'::timestamptz")
[ "$NA" = "1" ] && ok "(a) primo gradino consegnato davvero dopo l'upgrade" || no "(a) consegna" "$NA"
NB=$(one "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$I_B' AND at >= '$T0'::timestamptz")
[ "$NB" = "0" ] && ok "(b) nessun doppione del gradino già consegnato" || no "(b) doppione" "$NB"
SB=$(one "SELECT step FROM app.esc_alert_state WHERE incident_id='$I_B'")
[ "$SB" = "0" ] && ok "(b) gradino 1 non anticipato (dovuto dopo 300s)" || no "(b) step" "$SB"
NC=$(one "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$I_C' AND step=0")
[ "$NC" = "1" ] && ok "(c) escalation nativa avviata (nessuna perdita)" || no "(c)" "$NC"
# notifiche SIMULATE effettivamente arrivate al ricevitore dopo l'upgrade
NEWSINK=$(tail -c +$((SINK0+1)) "$SINK_LOG")
sk(){ echo "$NEWSINK" | grep -c "\"incident_id\":\"$1\""; }
[ "$(sk "$I_A")" -ge 1 ] && ok "notifica simulata ricevuta per (a) riavvio dal primo gradino" || no "sink (a)" "$(sk "$I_A")"
[ "$(sk "$I_C")" -ge 1 ] && ok "notifica simulata ricevuta per (c) possibile doppio avviso" || no "sink (c)" "$(sk "$I_C")"
[ "$(sk "$I_B")" = "0" ] && ok "nessuna notifica per (b) continuità (gradino già consegnato)" || no "sink (b)" "$(sk "$I_B")"
[ "$(sk "$I_TCD")" = "0" ] && [ "$(sk "$I_D")" = "0" ] && ok "nessuna notifica per chiusura e preso in carico" || no "sink tcd/d" "$(sk "$I_TCD")/$(sk "$I_D")"
NT=$(one "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$I_TCD' AND at >= '$T0'::timestamptz")
[ "$NT" = "0" ] && ok "traccar_conn_down: nessuna notifica dopo l'upgrade" || no "tcd notifiche" "$NT"

# --- bridge Teltonika simulato: IMEI conteso rifiutato, IMEI univoco accettato ---
R=$(TP=$TP IMEI=$DUP python - <<'PY'
import socket,struct,os
imei=os.environ["IMEI"].encode()
s=socket.create_connection(("127.0.0.1",int(os.environ["TP"])),timeout=5)
s.sendall(struct.pack(">H",len(imei))+imei); r=s.recv(1); print(r.hex() if r else "none"); s.close()
PY
)
[ "$R" = "00" ] && ok "bridge: IMEI registrato in 2 tenant rifiutato (0x00)" || no "bridge ambiguo" "$R"
grep -q "identità ambigua" "$LOG" && ok "bridge: rifiuto per ambiguità registrato nel log" || no "log ambiguità" "-"

kill $CPID 2>/dev/null; wait $CPID 2>/dev/null
[ -n "${KEEP:-}" ] || docker exec ywdb psql -U postgres -tAc "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
