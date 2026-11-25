#!/usr/bin/env bash
# SL-15 + SA-10 (decisioni PO 26/11/2026): catalogo dei tipi di evento e catalogo dei pacchetti nel DB,
# gestiti dal superadmin. Modifica e ripristina un nome; i pacchetti e i tenant di prova vengono eliminati.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))')
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
TIDS=()
cleanup(){ for t in "${TIDS[@]:-}"; do [ -n "$t" ] && sa DELETE "/api/superadmin/tenants/${t%%:*}" "{\"slug\":\"${t##*:}\"}" >/dev/null; done
  sa DELETE "/api/superadmin/plans/prova-$R" >/dev/null; [ -n "${ORIG:-}" ] && sa PUT /api/superadmin/event-types/batteria_bassa "$ORIG" >/dev/null; rm -rf "$J"; }
trap cleanup EXIT

# --- tipi di evento ---
ET=$(sa GET /api/superadmin/event-types)
echo "$ET" | py "[t for t in d['types'] if t['code']=='sos'][0]['life_safety'] and [t for t in d['types'] if t['code']=='sos'][0]['critical']" | grep -q True \
  && ok "SOS è salva-vita e critico (deciso dal prodotto)" || no "sos" "-"
echo "$ET" | py "len({t['category'] for t in d['types'] if t['code'] in ('device_offline','perdita_contatto','heartbeat_down','dispositivo_inattivo')})==1" | grep -q True \
  && ok "le quattro forme di «non comunica» sono una sola famiglia" || no "famiglia connessione" "-"
echo "$ET" | py "len({t['category'] for t in d['types'] if t['code'] in ('geofence_ingresso','geofence_uscita','ingresso_sito','presenza_ingresso')})==1" | grep -q True \
  && ok "le forme di «area» sono una sola famiglia" || no "famiglia area" "-"
ORIG=$(echo "$ET" | py "json.dumps({k:[t for t in d['types'] if t['code']=='batteria_bassa'][0][k] for k in ('label','category','description')})")
C=$(sac PUT /api/superadmin/event-types/batteria_bassa '{"label":"Batteria quasi scarica (prova)","category":"dispositivo","description":"prova"}')
[ "$C" = "200" ] && ok "il superadmin rinomina un tipo di evento" || no "rinomina" "$C"
[ "$(sac PUT /api/superadmin/event-types/batteria_bassa '{"label":"X y","category":"inventata"}')" = "400" ] && ok "famiglia sconosciuta: rifiutata" || no "famiglia" "-"
[ "$(sac PUT /api/superadmin/event-types/non_esiste '{"label":"Prova","category":"altro"}')" = "404" ] && ok "tipo inesistente: 404" || no "404" "-"

# tenant di prova: vede il nome nuovo, non può modificare
SLUG="ca$R"; TID=$(sa POST /api/superadmin/tenants "{\"slug\":\"$SLUG\",\"name\":\"Ca $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" | py "d.get('id','')")
TIDS+=("$TID:$SLUG")
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
AC=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" "$BASE/api/event-types" | py "[t['label'] for t in d['types'] if t['code']=='batteria_bassa'][0]" | grep -q "quasi scarica (prova)" \
  && ok "la console del tenant riceve il nome dal catalogo" || no "tenant label" "-"
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $AC" -H 'content-type: application/json' -X PUT "$BASE/api/superadmin/event-types/batteria_bassa" -d '{"label":"Hack","category":"altro"}')
[ "$C" = "401" ] || [ "$C" = "403" ] && ok "un amministratore di tenant non modifica il catalogo ($C)" || no "tenant write" "$C"
sa GET "/api/superadmin/tenants/$TID/modules" | py "[m['state'] for m in d if m['code']=='loneworker'][0]" | grep -q attivo \
  && ok "pacchetto predefinito «sicurezza» dal DB: modulo attivato" || no "plan sicurezza" "-"

# --- pacchetti ---
[ "$(sac POST /api/superadmin/plans "{\"code\":\"prova-$R\",\"name\":\"Prova $R\",\"modules\":[\"non_esiste\"]}")" = "400" ] && ok "modulo sconosciuto: rifiutato" || no "modulo sconosciuto" "-"
[ "$(sac POST /api/superadmin/plans "{\"code\":\"prova-$R\",\"name\":\"Prova $R\",\"desc\":\"solo ronde\",\"modules\":[\"giri\"]}")" = "200" ] && ok "nuovo pacchetto creato" || no "crea pacchetto" "-"
S2="cb$R"; T2=$(sa POST /api/superadmin/tenants "{\"slug\":\"$S2\",\"name\":\"Cb $R\",\"admin_email\":\"a@$S2.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"prova-$R\"}" | py "d.get('id','')"); TIDS+=("$T2:$S2")
sa GET "/api/superadmin/tenants/$T2/modules" | py "[m['state'] for m in d if m['code']=='giri'][0]" | grep -q attivo \
  && ok "tenant creato col pacchetto nuovo: moduli attivi" || no "tenant col pacchetto" "-"
sa PUT "/api/superadmin/plans/prova-$R" "{\"name\":\"Prova $R\",\"modules\":[\"giri\"],\"active\":false}" >/dev/null
C=$(sac POST /api/superadmin/tenants "{\"slug\":\"cc$R\",\"name\":\"Cc\",\"admin_email\":\"a@cc$R.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"prova-$R\"}")
[ "$C" = "400" ] && ok "pacchetto disattivato: non si usa più per nuovi tenant" || no "pacchetto disattivato" "$C"
[ "$(sac DELETE /api/superadmin/plans/full)" = "409" ] && ok "pacchetto predefinito: non si elimina" || no "elimina predefinito" "-"
[ "$(sac DELETE "/api/superadmin/plans/prova-$R")" = "200" ] && ok "pacchetto aggiunto: si elimina" || no "elimina" "-"
echo "catalogs: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
