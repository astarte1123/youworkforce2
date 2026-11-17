# IoT industriale nativo (YouWorkforce)

Il modulo **`iot`** ("Dispositivi e IoT industriale") è la piattaforma IoT **nativa** di YW:
assorbe le funzioni che prima si appoggiavano a ThingsBoard esterno, **senza alcun server
esterno** (coerente con la direzione self-contained; unica integrazione esterna resta l'AI).

## Cosa copre (nativo)

- **Inventario dispositivi** (core): censimento, onboarding, token di ingest, sede, assegnatario.
- **Telemetria time-series** (`device_telemetry`): storico per chiave/periodo.
- **Attributi** (`device_attributes`): `client` (dal device), `shared` (config verso il device),
  `server` (interni).
- **Comandi / RPC** (`iot_commands`): consegna diretta (edge Teltonika) o in coda (il device fa pull).
- **Soglie → incidenti**: le regole sensori (`sensor_rules`) valutate sulla telemetria creano
  incidenti nativi (centrale + escalation con Lone Worker attivo).
- **Scenari/Dashboard IoT**: casi d'uso applicabili per tenant.

## API native

| Scopo | Endpoint |
|---|---|
| Ingest telemetria | `POST /api/ingress/telemetry?token=…&id=…` body `{ts?, values:{k:v}, attributes?:{} }` |
| Pull device (config+comandi) | `GET /api/ingress/pull?token=…&id=…` → `{shared, commands}` (li marca `sent`) |
| Storico telemetria | `GET /api/iot/devices/:id/telemetry?key=&from=&to=&limit=` |
| Ultimi valori | `GET /api/iot/devices/:id/telemetry/latest` |
| Attributi | `GET/POST /api/iot/devices/:id/attributes` (`{scope, values}`) |
| Comando/RPC | `POST /api/iot/devices/:id/command` (`{method, params?}`) · `GET …/commands` |
| Soglie | `GET/POST /api/sensor-rules` (regole metric/op/threshold/etype) |

## Perimetro / integrazione

- Vive nel modulo **`iot`** (product). Autonomo: telemetria/attributi/comandi/soglie funzionano
  da soli.
- Riusa il **core**: inventario dispositivi, geofence, mappe, audit, RLS per-tenant, ingest token.
- Integra con **Lone Worker** (le soglie diventano incidenti con escalation), **Vigilanza/Cantieri**
  (i device sul campo), **Mappa dispositivi** (posizioni live).

## ThingsBoard esterno: ritirato

Il connettore `thingsboard` è stato eliminato dal catalogo (mig 0108) e le sue route proxy
rimosse. Il vecchio manuale resta come riferimento storico in `docs/thingsboard-manuale.md`.

## Tenant demo

`iot` (IoT Demo SpA, `admin@iot.loneworker.eu` / `demo`) con dispositivi e telemetria d'esempio.
