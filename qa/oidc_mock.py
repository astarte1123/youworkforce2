"""Finto Autentika (OpenID Connect) per i test del login con Autentika. Nessun accesso a internet.
Issuer http://127.0.0.1:<porta>, chiave ES256 generata all'avvio (kid k1), client «ywf-test» con
segreto «s3cret». Simula il contratto di docs/integration/oidc-rp.md di Autentika:
- /.well-known/openid-configuration, /.well-known/jwks.json
- /oauth2/auth: PKCE S256 obbligatorio; l'utente che «entra» è quello impostato con POST /_mock/user;
  POST /_mock/mode {"mode": "deny" | "bad_nonce" | "bad_sig" | "ok"} per provare gli errori
- /oauth2/token: client_secret_basic, verifica del code_verifier e del redirect_uri, id_token ES256
  (typ JWT) con sid e, se c'è la risorsa, access token (typ at+jwt, aud = risorsa, roles)
- /v1/auth/me/logout: registra la sessione chiusa (id_token_hint)
- /_mock/logout_token?sid=...: logout_token firmato (typ logout+jwt) da mandare al back-channel
- /_mock/log: le chiusure di sessione ricevute
Uso: python -I qa/oidc_mock.py [porta=18097]"""
import base64, hashlib, json, secrets, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs, urlencode
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18097
ISS = f'http://127.0.0.1:{PORT}'
CLIENT, SECRET = 'ywf-test', 's3cret'
KEY = ec.generate_private_key(ec.SECP256R1()); OTHER = ec.generate_private_key(ec.SECP256R1())
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b'=').decode()
nums = KEY.public_key().public_numbers()
JWK = {'kty': 'EC', 'crv': 'P-256', 'kid': 'k1', 'alg': 'ES256', 'use': 'sig', 'x': b64(nums.x.to_bytes(32, 'big')), 'y': b64(nums.y.to_bytes(32, 'big'))}
USER = {'sub': 'user_00000000-0000-0000-0000-000000000001', 'email': 'nuovo@example.test', 'email_verified': True, 'name': 'Nuovo', 'roles': ['member']}
MODE = {'mode': 'ok'}
CODES, LOG = {}, []

def sign(claims, typ, key=None):
    h = b64(json.dumps({'alg': 'ES256', 'typ': typ, 'kid': 'k1'}).encode()); p = b64(json.dumps(claims).encode())
    der = (key or KEY).sign(f'{h}.{p}'.encode(), ec.ECDSA(hashes.SHA256())); r, s = decode_dss_signature(der)
    return f'{h}.{p}.{b64(r.to_bytes(32, "big") + s.to_bytes(32, "big"))}'

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, code, body=None, headers=None):
        self.send_response(code)
        for k, v in (headers or {}).items(): self.send_header(k, v)
        data = json.dumps(body).encode() if body is not None else b''
        if body is not None: self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
    def form(self):
        n = int(self.headers.get('Content-Length') or 0); raw = self.rfile.read(n).decode()
        if 'json' in (self.headers.get('Content-Type') or ''): return json.loads(raw or '{}')
        return {k: v[0] for k, v in parse_qs(raw).items()}
    def do_GET(self):
        u = urlparse(self.path); q = {k: v[0] for k, v in parse_qs(u.query).items()}
        if u.path == '/.well-known/openid-configuration':
            return self.send(200, {'issuer': ISS, 'authorization_endpoint': ISS + '/oauth2/auth', 'token_endpoint': ISS + '/oauth2/token',
                'jwks_uri': ISS + '/.well-known/jwks.json', 'end_session_endpoint': ISS + '/v1/auth/me/logout', 'userinfo_endpoint': ISS + '/userinfo',
                'code_challenge_methods_supported': ['S256'], 'response_types_supported': ['code'], 'id_token_signing_alg_values_supported': ['ES256'],
                'backchannel_logout_supported': True})
        if u.path == '/.well-known/jwks.json': return self.send(200, {'keys': [JWK]})
        if u.path == '/oauth2/auth':
            if q.get('client_id') != CLIENT: return self.send(400, {'error': 'invalid_client'})
            if q.get('code_challenge_method') != 'S256' or not q.get('code_challenge'): return self.send(400, {'error': 'invalid_request'})
            ru, st = q['redirect_uri'], q.get('state', '')
            if MODE['mode'] == 'deny':
                return self.send(302, headers={'Location': ru + '?' + urlencode({'error': 'access_denied', 'error_description': 'no role', 'state': st})})
            code = secrets.token_urlsafe(16)
            CODES[code] = {'q': q, 'user': dict(USER), 'mode': MODE['mode'], 'sid': 'sid_' + secrets.token_hex(6)}
            return self.send(302, headers={'Location': ru + '?' + urlencode({'code': code, 'state': st})})
        if u.path == '/_mock/logout_token':
            now = int(time.time())
            return self.send(200, {'logout_token': sign({'iss': ISS, 'aud': q.get('aud', CLIENT), 'iat': now, 'exp': now + 120, 'jti': secrets.token_hex(8),
                'sid': q.get('sid'), 'events': {'http://schemas.openid.net/event/backchannel-logout': {}}}, 'logout+jwt')})
        if u.path == '/_mock/log': return self.send(200, LOG)
        self.send(404, {'error': 'not_found'})
    def do_POST(self):
        u = urlparse(self.path); f = self.form()
        if u.path == '/_mock/user': USER.clear(); USER.update(f); return self.send(200, {'ok': True})
        if u.path == '/_mock/mode': MODE['mode'] = f.get('mode', 'ok'); return self.send(200, {'ok': True})
        if u.path == '/v1/auth/me/logout':
            LOG.append({'client_id': f.get('client_id'), 'id_token_hint': bool(f.get('id_token_hint'))}); return self.send(200, {'status': 'logged_out'})
        if u.path == '/oauth2/token':
            auth = self.headers.get('Authorization', '')
            if not auth.startswith('Basic ') or base64.b64decode(auth[6:]).decode() != f'{CLIENT}:{SECRET}': return self.send(401, {'error': 'invalid_client'})
            c = CODES.pop(f.get('code', ''), None)
            if not c: return self.send(400, {'error': 'invalid_grant'})
            q = c['q']
            if f.get('redirect_uri') != q['redirect_uri']: return self.send(400, {'error': 'invalid_grant'})
            if b64(hashlib.sha256(f.get('code_verifier', '').encode()).digest()) != q['code_challenge']: return self.send(400, {'error': 'invalid_grant'})
            now = int(time.time()); usr = c['user']
            idc = {'iss': ISS, 'sub': usr['sub'], 'aud': CLIENT, 'iat': now, 'exp': now + 300, 'jti': secrets.token_hex(8),
                   'nonce': 'altro' if c['mode'] == 'bad_nonce' else q.get('nonce'), 'sid': c['sid'], 'email': usr.get('email'),
                   'email_verified': usr.get('email_verified') in (True, 'true', '1', 1), 'name': usr.get('name'), 'groups': []}
            out = {'token_type': 'Bearer', 'expires_in': 300, 'id_token': sign(idc, 'JWT', OTHER if c['mode'] == 'bad_sig' else None), 'refresh_token': secrets.token_urlsafe(16)}
            if q.get('resource'):
                roles = usr.get('roles') or ['member']
                if isinstance(roles, str): roles = [r for r in roles.split(',') if r]
                out['access_token'] = sign({'iss': ISS, 'sub': usr['sub'], 'aud': q['resource'], 'iat': now, 'exp': now + 300, 'nbf': now, 'jti': secrets.token_hex(8),
                    'client_id': CLIENT, 'tenant_id': '11111111-1111-1111-1111-111111111111', 'principal_type': 'user', 'roles': roles, 'permissions': []}, 'at+jwt')
            return self.send(200, out)
        self.send(404, {'error': 'not_found'})

ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
