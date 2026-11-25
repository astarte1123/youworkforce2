#!/usr/bin/env bash
# TA-11 / CA-19 (mig 0144): un riferimento deve puntare a un dato della STESSA azienda.
# Con l'id di un utente, una sede o un gruppo di un'altra azienda le scritture vengono rifiutate
# (400, messaggio chiaro), qualunque sia la via (API, agente, worker): controllo nel database.
# Due tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SA_="xa$R"; SB_="xb$R"
mk(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$1\",\"name\":\"X $1\",\"admin_email\":\"a@$1.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"giri\",\"loneworker\"]}" | py "d.get('id','')"; }
TA=$(mk "$SA_"); TB=$(mk "$SB_")
del(){ curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$1" -d "{\"slug\":\"$2\"}"; }
trap 'del "$TA" "$SA_"; del "$TB" "$SB_"; rm -rf "$J"' EXIT
[ -n "$TA" ] && [ -n "$TB" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $1" -d "{\"email\":\"a@$1.test\",\"password\":\"DirectPass9\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $1" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o "$J/last" -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $1" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login "$SA_"; login "$SB_"
UA=$(call "$SA_" POST /api/users '{"email":"t@a.test","role":"lavoratore","password":"WorkPass9"}' | py "d['id']")
UB=$(call "$SB_" POST /api/users '{"email":"t@b.test","role":"lavoratore","password":"WorkPass9"}' | py "d['id']")
SB=$(call "$SB_" POST /api/sites '{"name":"Sede B"}' | py "d['id']"); SA=$(call "$SA_" POST /api/sites '{"name":"Sede A"}' | py "d['id']")
GA=$(call "$SA_" POST /api/user-groups '{"name":"Squadra A"}' | py "d['id']")
GB=$(call "$SB_" POST /api/user-groups '{"name":"Squadra B"}' | py "d['id']")

echo "== riferimenti a un'altra azienda =="
C=$(code "$SA_" POST "/api/user-groups/$GA/members" "{\"user_id\":\"$UB\"}"); [ "$C" = "400" ] && ok "gruppo: utente di un'altra azienda rifiutato (400)" || no "membro altrui" "$C $(cat "$J/last")"
grep -q "non appartiene alla tua azienda" "$J/last" && ok "messaggio chiaro" || no "messaggio" "$(cat "$J/last")"
[ "$(q "SELECT count(*) FROM app.user_group_members WHERE group_id='$GA' AND user_id='$UB'")" = "0" ] && ok "nessuna riga scritta" || no "riga" "-"
C=$(code "$SA_" POST /api/orders "{\"title\":\"X\",\"assigned_user\":\"$UB\"}"); [ "$C" = "400" ] && ok "ordine assegnato a un tecnico di un'altra azienda: 400" || no "ordine" "$C"
C=$(code "$SA_" POST /api/orders "{\"title\":\"X\",\"site_id\":\"$SB\"}"); [ "$C" = "400" ] && ok "ordine su una sede di un'altra azienda: 400" || no "ordine sede" "$C"
C=$(code "$SA_" POST "/api/users/$UA/sites" "{\"site_ids\":[\"$SB\"]}"); [ "$C" = "400" ] && ok "sedi utente: sede di un'altra azienda rifiutata" || no "sedi" "$C"
C=$(code "$SA_" POST /api/patrol-routes "{\"name\":\"G\",\"site_id\":\"$SB\"}"); [ "$C" = "400" ] && ok "giro su una sede di un'altra azienda: 400" || no "giro" "$C"
C=$(code "$SA_" POST /api/patrol-routes "{\"name\":\"G2\",\"assigned_group\":\"$GB\"}"); [ "$C" = "400" ] && ok "giro assegnato a un gruppo di un'altra azienda: 400" || no "giro gruppo" "$C"

echo "== stessa azienda: tutto normale =="
C=$(code "$SA_" POST "/api/user-groups/$GA/members" "{\"user_id\":\"$UA\"}"); [ "$C" = "200" ] && ok "membro della stessa azienda: 200" || no "membro" "$C"
C=$(code "$SA_" POST /api/orders "{\"title\":\"Y\",\"assigned_user\":\"$UA\",\"site_id\":\"$SA\"}"); [ "$C" = "200" ] && ok "ordine nella stessa azienda: 200" || no "ordine ok" "$C"
C=$(code "$SA_" POST "/api/users/$UA/sites" "{\"site_ids\":[\"$SA\"]}"); [ "$C" = "200" ] && ok "sede della stessa azienda: 200" || no "sedi ok" "$C"
[ "$(q "SELECT count(*) FROM pg_trigger WHERE tgname='yw_same_tenant'")" -ge 80 ] && ok "controllo attivo su tutte le tabelle collegate" || no "trigger" "-"

echo "cross-tenant-refs: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
