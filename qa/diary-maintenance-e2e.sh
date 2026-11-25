#!/usr/bin/env bash
# CA-20 (decisioni PO 26/11/2026): un solo diario (giornale di cantiere dentro diary_entries) e
# manutenzioni generate da sole. Tenant usa-e-getta; la generazione si forza con l'hook di test.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -Atc "$1"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="dm$R"
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Dm $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"cantieri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }

# --- un solo diario ---
[ "$(sql "SELECT to_regclass('app.commessa_diario') IS NULL")" = "t" ] && ok "la tabella del vecchio giornale di cantiere non esiste più" || no "tabella vecchia" "-"
CID=$(call POST /api/commesse "{\"code\":\"C-$R\",\"name\":\"Cantiere prova\"}" | py "d.get('id','')")
call POST "/api/commesse/$CID/diario" '{"body":"Getto fondazioni","weather":"sereno","present_count":5,"diary_date":"2026-11-20"}' >/dev/null
call GET "/api/commesse/$CID" | py "[(x['body'],x['weather'],x['present_count'],str(x['diary_date'])) for x in d['diario']]" | grep -q "Getto fondazioni', 'sereno', 5, '2026-11-20'" \
  && ok "nota del giornale di cantiere con data, meteo e presenti" || no "giornale" "$(call GET /api/commesse/$CID | py "d.get('diario')")"
[ "$(sql "SELECT count(*) FROM app.diary_entries WHERE tenant_id='$TID' AND commessa_id='$CID' AND source='cantiere'")" = "1" ] \
  && ok "la nota sta nel diario unico (diary_entries, origine cantiere)" || no "diario unico" "-"
call GET /api/diary | py "any(x.get('commessa_id')=='$CID' for x in d)" | grep -q True && ok "il diario generale mostra anche la nota di cantiere" || no "diario generale" "-"
WID=$(call POST /api/diary '{"body":"nota di lavoro"}' | py "d.get('id','')")
call DELETE "/api/commessa-diario/$WID" >/dev/null
[ "$(sql "SELECT count(*) FROM app.diary_entries WHERE id='$WID'")" = "1" ] && ok "dal giornale di cantiere non si cancellano note di altri diari" || no "cancellazione estranea" "-"
GID=$(call GET "/api/commesse/$CID" | py "d['diario'][0]['id']")
call DELETE "/api/commessa-diario/$GID" >/dev/null
[ "$(call GET "/api/commesse/$CID" | py "len(d['diario'])")" = "0" ] && ok "nota di cantiere eliminata" || no "elimina" "-"

# --- manutenzioni generate da sole ---
NOW=$(curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | py "d['now']")
D(){ python -c "import datetime,zoneinfo,sys;n=datetime.datetime.fromisoformat(sys.argv[1].replace('Z','+00:00')).astimezone(zoneinfo.ZoneInfo('Europe/Rome')).date();print(n+datetime.timedelta(days=int(sys.argv[2])))" "$NOW" "$1"; }
P1=$(call POST /api/maintenance-plans "{\"title\":\"Caldaia\",\"every_days\":30,\"next_date\":\"$(D 3)\",\"lead_days\":7}" | py "d['id']")
P2=$(call POST /api/maintenance-plans "{\"title\":\"Estintori\",\"every_days\":30,\"next_date\":\"$(D 3)\",\"auto_generate\":false}" | py "d['id']")
P3=$(call POST /api/maintenance-plans "{\"title\":\"Lontana\",\"every_days\":30,\"next_date\":\"$(D 40)\",\"lead_days\":7}" | py "d['id']")
P4=$(call POST /api/maintenance-plans "{\"title\":\"Arretrata\",\"every_days\":30,\"next_date\":\"$(D -100)\"}" | py "d['id']")
curl -s -o /dev/null -X POST "$BASE/api/_test/maintenance-autogen"
cnt(){ sql "SELECT count(*) FROM app.work_orders WHERE tenant_id='$TID' AND title='$1 (manutenzione)'"; }
nd(){ sql "SELECT next_date FROM app.maintenance_plans WHERE id='$1'"; }
[ "$(cnt Caldaia)" = "1" ] && [ "$(nd "$P1")" = "$(D 33)" ] && ok "scadenza entro l'anticipo: intervento creato da solo, data avanzata" || no "auto" "$(cnt Caldaia) $(nd "$P1")"
[ "$(cnt Estintori)" = "0" ] && ok "piano senza generazione automatica: nessun intervento" || no "manuale" "$(cnt Estintori)"
[ "$(cnt Lontana)" = "0" ] && ok "scadenza lontana: ancora niente" || no "lontana" "$(cnt Lontana)"
[ "$(cnt Arretrata)" = "1" ] && python -c "import sys;sys.exit(0 if '$(nd "$P4")'>='$(D 0)' else 1)" && ok "piano arretrato: un solo intervento e prossima data nel futuro" || no "arretrata" "$(cnt Arretrata) $(nd "$P4")"
curl -s -o /dev/null -X POST "$BASE/api/_test/maintenance-autogen"
[ "$(cnt Caldaia)" = "1" ] && [ "$(cnt Arretrata)" = "1" ] && ok "una seconda passata non duplica" || no "duplicati" "$(cnt Caldaia)/$(cnt Arretrata)"
H=$(sql "SELECT to_char(scheduled_start AT TIME ZONE 'Europe/Rome','HH24:MI') FROM app.work_orders WHERE tenant_id='$TID' AND title='Caldaia (manutenzione)'")
[ "$H" = "09:00" ] && ok "appuntamento alle 9:00 nell'ora dell'azienda" || no "ora" "$H"
call GET /api/maintenance-plans | py "[(p['auto_generate'],p['lead_days']) for p in d if p['id']=='$P2'][0]" | grep -q "False, 7" && ok "il piano mostra se si genera da solo e con quanto anticipo" || no "elenco" "-"
echo "diary-maintenance: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
