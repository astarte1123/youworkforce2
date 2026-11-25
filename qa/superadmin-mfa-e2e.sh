#!/usr/bin/env bash
# SA-12 (decisione PO 25/11/2026): account superadmin gestiti dalla UI con verifica in due passaggi.
#  - creazione di un superadmin (password >= 12), attivazione TOTP, accesso con codice
#  - senza codice / codice errato / stesso codice riusato -> rifiuto
#  - disattivazione (non di sé stessi), azzeramento MFA, cambio password, audit
# Il codice si calcola sull'orologio del server (in sviluppo è l'orologio di test).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
R=$(python -c 'import random;print(random.randint(100000,999999))'); EM="sa$R@e2e.test"; PW="Password-e2e-$R"
trap 'docker exec -i ywdb psql -U postgres -d youworkforce -qc "DELETE FROM app.sessions WHERE email='"'"'$EM'"'"'; DELETE FROM app.superadmins WHERE email='"'"'$EM'"'"'" >/dev/null; rm -rf "$J"' EXIT
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
login(){ curl -s -c "$J/n" -w "\n%{http_code}" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d "$1"; }
NC(){ awk '$6=="yw_csrf"{print $7}' "$J/n" | tail -1; }
na(){ curl -s -b "$J/n" -H "x-csrf-token: $(NC)" -H 'content-type: application/json' "$@"; }
srvnow(){ curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}' | py "d['now']"; }
totp(){ python - "$1" "$2" "${3:-0}" <<'PY'
import sys,hmac,hashlib,base64,struct,datetime
sec,now,off=sys.argv[1],sys.argv[2],int(sys.argv[3])
t=datetime.datetime.fromisoformat(now.replace('Z','+00:00')[:26]+'+00:00').timestamp()
k=base64.b32decode(sec+'='*(-len(sec)%8)); step=int(t//30)+off
h=hmac.new(k,struct.pack('>Q',step),hashlib.sha1).digest(); o=h[19]&15
print('%06d'%((struct.unpack('>I',h[o:o+4])[0]&0x7fffffff)%1000000))
PY
}

C=$(sac -X POST "$BASE/api/superadmin/admins" -d "{\"email\":\"$EM\",\"password\":\"corta\"}"); [ "$C" = "400" ] && ok "password sotto i 12 caratteri rifiutata" || no "pw corta" "$C"
NID=$(sa -X POST "$BASE/api/superadmin/admins" -d "{\"email\":\"$EM\",\"password\":\"$PW\"}" | py "d['id']")
[ -n "$NID" ] && ok "nuovo superadmin creato" || no "crea" "-"
C=$(sac -X POST "$BASE/api/superadmin/admins" -d "{\"email\":\"$EM\",\"password\":\"$PW\"}"); [ "$C" = "409" ] && ok "email doppia: 409" || no "dup" "$C"

OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\"}"); [ "$(echo "$OUT" | tail -1)" = "200" ] && ok "primo accesso con la password" || no "login" "$OUT"
SEC=$(na -X POST "$BASE/api/superadmin/mfa/setup" | py "d['secret']")
[ ${#SEC} -ge 32 ] && ok "segreto TOTP generato (chiave da inserire nell'app)" || no "setup" "$SEC"
case "$(q "SELECT totp_pending FROM app.superadmins WHERE email='$EM'")" in enc:v3:*) ok "segreto in attesa cifrato nel DB";; *) no "cifratura" "-";; esac
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/n" -H "x-csrf-token: $(NC)" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/mfa/enable" -d '{"code":"000000"}')
[ "$C" = "400" ] && ok "codice di attivazione errato rifiutato" || no "enable bad" "$C"
NOW=$(srvnow); CODE=$(totp "$SEC" "$NOW")
na -X POST "$BASE/api/superadmin/mfa/enable" -d "{\"code\":\"$CODE\"}" | grep -q '"ok":true' && ok "verifica in due passaggi attivata" || no "enable" "-"

OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\"}")
[ "$(echo "$OUT" | tail -1)" = "401" ] && echo "$OUT" | grep -q mfa_required && ok "senza codice: 401 con richiesta del codice" || no "no otp" "$OUT"
OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\",\"otp\":\"123456\"}"); [ "$(echo "$OUT" | tail -1)" = "401" ] && ok "codice errato: 401" || no "bad otp" "$OUT"
OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\",\"otp\":\"$CODE\"}"); [ "$(echo "$OUT" | tail -1)" = "401" ] && ok "lo stesso codice non vale due volte (anti-replay)" || no "replay" "$OUT"
NEXT=$(totp "$SEC" "$NOW" 1)
OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\",\"otp\":\"$NEXT\"}"); [ "$(echo "$OUT" | tail -1)" = "200" ] && ok "password + codice valido: accesso" || no "otp ok" "$OUT"
na "$BASE/api/me" | py "d['is_super']" | grep -q True && ok "sessione completa del piano di controllo" || no "sessione" "-"

sa "$BASE/api/superadmin/admins" | py "[a['mfa'] for a in d['admins'] if a['email']=='$EM'][0]" | grep -q True && ok "elenco account: MFA attiva" || no "elenco" "-"
MYID=$(sa "$BASE/api/superadmin/admins" | py "d['me']")
C=$(sac -X POST "$BASE/api/superadmin/admins/$MYID" -d '{"disabled":true}'); [ "$C" = "400" ] && ok "non ci si può disattivare da soli" || no "self disable" "$C"
sa -X POST "$BASE/api/superadmin/admins/$NID" -d '{"disabled":true}' >/dev/null
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/n" "$BASE/api/superadmin/tenants"); [ "$C" = "401" ] && ok "account disattivato: le sue sessioni non valgono più" || no "sessione dopo disattivazione" "$C"
OUT=$(login "{\"email\":\"$EM\",\"password\":\"$PW\",\"otp\":\"$(totp "$SEC" "$NOW" 2)\"}"); [ "$(echo "$OUT" | tail -1)" = "403" ] && ok "account disattivato: accesso rifiutato" || no "login disattivato" "$OUT"
sa -X POST "$BASE/api/superadmin/admins/$NID" -d '{"disabled":false,"reset_mfa":true}' >/dev/null
[ "$(q "SELECT totp_secret IS NULL FROM app.superadmins WHERE email='$EM'")" = "t" ] && ok "MFA azzerata da un altro superadmin (es. telefono perso)" || no "reset" "-"
C=$(sac -X POST "$BASE/api/superadmin/password" -d '{"current":"sbagliata-sbagliata","new":"Nuova-password-123"}'); [ "$C" = "403" ] && ok "cambio della propria password con quella attuale errata: 403" || no "own pw" "$C"
N=$(q "SELECT count(*) FROM app.audit_log WHERE actor LIKE 'superadmin:%' AND detail->>'account'='$EM' AND action IN ('superadmin_create','superadmin_mfa_enabled','superadmin_disable','superadmin_mfa_reset')")
[ "$N" = "4" ] && ok "creazione, attivazione MFA, disattivazione e azzeramento nel registro attività" || no "audit" "$N"
echo "superadmin-mfa: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
