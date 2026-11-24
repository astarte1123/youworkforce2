#!/usr/bin/env bash
# SA-7 + SA-8: hardening superadmin.
#  SA-8 logo: solo PNG/JPEG/WebP riconosciuti dai byte; SVG/HTML rifiutati anche se dichiarati
#        image/png; oltre 512 KB rifiutato; servito con nosniff + CSP.
#  SA-7 impersonazione: solo un amministratore reale (altrimenti 409); all'uscita la sessione
#        superadmin porta l'id del superadmin, non quello dell'utente impersonato.
# Tenant usa-e-getta (creato ed eliminato dalla suite). Mai acme/globex.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
S(){ curl -s -b "$J/sa" -H "x-csrf-token: $CS" "$@"; }
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="hard$R"
TID=$(S -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Hard $R\",\"admin_email\":\"admin@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
cleanup(){ [ -n "$TID" ] && S -H 'content-type: application/json' -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}" -o /dev/null; rm -rf "$J"; }
trap cleanup EXIT
[ -n "$TID" ] && ok "tenant usa-e-getta creato" || { no "crea tenant" "-"; exit 1; }

echo "== SA-8 logo =="
up(){ S -o /dev/null -w "%{http_code}" -X POST "$BASE/api/superadmin/tenants/$TID/logo" -H "content-type: $1" --data-binary "@$2"; }
printf '<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>' > "$J/x.svg"
printf '<html><script>alert(1)</script></html>' > "$J/x.html"
python -c "import sys;sys.stdout.buffer.write(bytes([0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A])+b'\0'*64)" > "$J/x.png"
python -c "import sys;sys.stdout.buffer.write(bytes([0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A])+b'\0'*(600*1024))" > "$J/big.png"
C=$(up image/svg+xml "$J/x.svg"); [ "$C" = "415" ] && ok "SVG rifiutato (415)" || no "svg" "$C"
C=$(up image/png "$J/x.svg"); [ "$C" = "415" ] && ok "SVG dichiarato image/png rifiutato (415)" || no "svg-as-png" "$C"
C=$(up image/png "$J/x.html"); [ "$C" = "415" ] && ok "HTML dichiarato image/png rifiutato (415)" || no "html" "$C"
C=$(up image/png "$J/big.png"); [ "$C" = "413" ] && ok "oltre 512 KB rifiutato (413)" || no "big" "$C"
C=$(up application/octet-stream "$J/x.png"); [ "$C" = "200" ] && ok "PNG accettato (tipo dai byte, non dall'intestazione)" || no "png" "$C"
H=$(S -D - -o /dev/null "$BASE/api/superadmin/tenants/$TID/logo" | tr -d '\r' | tr 'A-Z' 'a-z')
echo "$H" | grep -q "^content-type: image/png" && ok "servito come image/png" || no "ct" "$(echo "$H" | grep content-type)"
echo "$H" | grep -q "^x-content-type-options: nosniff" && ok "servito con nosniff" || no "nosniff" "-"
echo "$H" | grep -q "^content-security-policy: default-src 'none'" && ok "servito con CSP chiusa" || no "csp" "-"
# logo legacy non riconosciuto (es. SVG caricato prima della correzione): non viene servito
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.tenants SET logo=convert_to('<svg><script>alert(1)</script></svg>','UTF8'), logo_ct='image/svg+xml' WHERE id='$TID'" >/dev/null
C=$(S -o /dev/null -w "%{http_code}" "$BASE/api/superadmin/tenants/$TID/logo"); [ "$C" = "404" ] && ok "SVG legacy non servito (404)" || no "legacy" "$C"

echo "== SA-7 impersonazione =="
SAID=$(q "SELECT id FROM app.superadmins WHERE email='admin@loneworker.eu'")
C=$(S -o /dev/null -w "%{http_code}" -c "$J/imp" -X POST "$BASE/api/superadmin/tenants/$TID/impersonate"); [ "$C" = "200" ] && ok "impersona l'amministratore reale" || no "imp" "$C"
IC=$(awk '$6=="yw_csrf"{print $7}' "$J/imp" | tail -1)
C=$(curl -s -o /dev/null -w "%{http_code}" -b "$J/imp" -c "$J/back" -H "x-csrf-token: $IC" -X POST "$BASE/api/impersonate/stop"); [ "$C" = "200" ] && ok "uscita dall'impersonazione" || no "stop" "$C"
TOK=$(awk '$6=="yw_session"{print $7}' "$J/back" | tail -1)
U=$(q "SELECT user_id FROM app.sessions WHERE token='$TOK'")
[ "$U" = "$SAID" ] && ok "sessione di ritorno con l'id del superadmin" || no "id ritorno" "$U vs $SAID"
docker exec -i ywdb psql -U postgres -d youworkforce -tAc "UPDATE app.users SET role='lavoratore' WHERE tenant_id='$TID'" >/dev/null
C=$(S -o /dev/null -w "%{http_code}" -X POST "$BASE/api/superadmin/tenants/$TID/impersonate"); [ "$C" = "409" ] && ok "tenant senza amministratore: impersonazione rifiutata (409)" || no "no-admin" "$C"

echo "superadmin-hardening: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
