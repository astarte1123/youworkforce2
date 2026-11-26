"""Finto api.met.no per i test (Locationforecast 2.0 «complete»): nessun accesso a internet.
- previsione oraria per 48 ore dall'ora corrente, poi a 6 ore fino a 9 giorni;
- pioggia 2 mm/h domani dalle 08 alle 12 UTC, vento 5 m/s, raffiche 10 m/s;
- Expires a 1 ora (il core deve comunque aspettare 3 ore), Last-Modified fisso;
- If-Modified-Since uguale a Last-Modified -> 304;
- lat=1.0000 -> 500 (per provare gli errori);
- registra ogni richiesta (percorso, User-Agent, If-Modified-Since) nel file passato come argomento.
Uso: python -I qa/metno_mock.py <log> [porta=18098]"""
import json, sys, datetime as dt
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from email.utils import format_datetime
from urllib.parse import urlparse, parse_qs

LOG = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 18098
LAST_MOD = 'Wed, 25 Nov 2026 10:00:00 GMT'

def forecast():
    now = dt.datetime.now(dt.timezone.utc).replace(minute=0, second=0, microsecond=0)
    rain_day = (now + dt.timedelta(days=1)).date()
    ts = []
    t = now
    while t < now + dt.timedelta(hours=48):
        rain = 2.0 if t.date() == rain_day and 8 <= t.hour < 12 else 0.0
        ts.append({'time': t.strftime('%Y-%m-%dT%H:%M:%SZ'), 'data': {
            'instant': {'details': {'air_temperature': 8.0, 'wind_speed': 5.0, 'wind_speed_of_gust': 10.0}},
            'next_1_hours': {'summary': {'symbol_code': 'rain' if rain else 'partlycloudy_day'}, 'details': {'precipitation_amount': rain, 'probability_of_precipitation': 90 if rain else 10}},
            'next_6_hours': {'summary': {'symbol_code': 'cloudy'}, 'details': {'precipitation_amount': 0.0}}}})
        t += dt.timedelta(hours=1)
    t = t.replace(hour=(t.hour // 6) * 6) + dt.timedelta(hours=6)
    while t < now + dt.timedelta(days=9):
        ts.append({'time': t.strftime('%Y-%m-%dT%H:%M:%SZ'), 'data': {
            'instant': {'details': {'air_temperature': 6.0, 'wind_speed': 3.0}},
            'next_6_hours': {'summary': {'symbol_code': 'clearsky_day'}, 'details': {'precipitation_amount': 0.0, 'probability_of_precipitation': 5}}}})
        t += dt.timedelta(hours=6)
    return {'type': 'Feature', 'properties': {'meta': {'updated_at': LAST_MOD}, 'timeseries': ts}}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        u = urlparse(self.path); q = parse_qs(u.query)
        with open(LOG, 'a', encoding='utf-8') as f:
            f.write(json.dumps({'path': u.path, 'lat': q.get('lat', [''])[0], 'lon': q.get('lon', [''])[0],
                                'ua': self.headers.get('User-Agent', ''), 'ims': self.headers.get('If-Modified-Since', '')}) + '\n')
        if u.path != '/weatherapi/locationforecast/2.0/complete':
            self.send_response(404); self.end_headers(); return
        if q.get('lat', [''])[0] == '1.0000':
            self.send_response(500); self.end_headers(); return
        if self.headers.get('If-Modified-Since') == LAST_MOD:
            self.send_response(304); self.end_headers(); return
        body = json.dumps(forecast()).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Expires', format_datetime(dt.datetime.now(dt.timezone.utc) + dt.timedelta(hours=1), usegmt=True))
        self.send_header('Last-Modified', LAST_MOD)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)

ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
