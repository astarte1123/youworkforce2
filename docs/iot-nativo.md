# IoT industriale nativo (YouWorkforce)

Il modulo **`iot`** ("Dispositivi e IoT industriale") è la piattaforma IoT **nativa** di YW:
assorbe le funzioni che prima si appoggiavano a ThingsBoard esterno, **senza alcun server
esterno** (coerente con la direzione self-contained; unica integrazione esterna resta l'AI).

## Cosa copre (nativo)

- **Inventario dispositivi** (core): censimento, onboarding, token di ingest, sede, assegnatario.
- **Telemetria time-series** (`device_telemetry`): storico per chiave/periodo.
- **Normalizzazione dati** (Fase 1): metriche **canoniche** (`iot_metrics`: temperature/energy/level/…
  con unità) + **mappa per-device** (`device_metric_map`: chiave grezza → metrica + trasformazione
  lineare `scala·x+offset`). All'ingest ogni valore mappato viene salvato anche con metrica+unità e
  le soglie si valutano sulle **chiavi canoniche** (una regola su `temperature` scatta a prescindere
  da come il device nomina il valore: `t`/`temp`/…). Senza mappa il dato resta grezzo (retro-compat).
- **Attributi** (`device_attributes`): `client` (dal device), `shared` (config verso il device),
  `server` (interni).
- **Comandi / RPC** (`iot_commands`): consegna diretta (edge Teltonika) o in coda (il device fa pull).
- **Soglie → alert IoT** (Fase 3): le regole sensori (`sensor_rules`) valutate sulla telemetria
  alzano un **alert IoT** in **vista separata** (`iot_alerts`, non la Centrale sicurezza). In
  **coesistenza** col modulo Lone Worker la soglia alza **anche** un incidente di sicurezza
  (centrale + escalation); un tenant puramente IoT resta nel solo mondo alert.
- **Alert IoT in vista separata** (Fase 3, vedi sotto): soglia / offline / ripristino, con ciclo
  di vita proprio (aperto → preso in carico → chiuso) e **watchdog offline** nativo.
- **Blueprint / attivazione zero-touch** (Fase 2): l'admin sceglie *che lavoro* deve fare il device
  (`iot_blueprints`: catena del freddo, livello serbatoio, energia, qualità aria, varchi, conteggio,
  ambiente LoRaWAN) e `POST /api/iot/activate` configura TUTTO da solo — device + token device-bound
  + mappa di normalizzazione + soglie di default — e restituisce le **istruzioni di collegamento** per
  il protocollo (HTTP / MQTT / LoRaWAN). "Quale device per quale lavoro; il resto si sistema."
- **Protocolli di ingest**: HTTP nativo (`/api/ingress/telemetry`), webhook **LoRaWAN/TTN**
  (`/api/ingress/lorawan`, payload decodificato → stessa pipeline), **MQTT nativo** (broker YW
  su TCP 1883, vedi sotto). Tutti confluiscono nella stessa pipeline `persist_telemetry`.
- **Scenari/Dashboard IoT**: casi d'uso applicabili per tenant.

## API native

| Scopo | Endpoint |
|---|---|
| Ingest telemetria | `POST /api/ingress/telemetry?token=…&id=…` body `{ts?, values:{k:v}, attributes?:{} }` |
| Pull device (config+comandi) | `GET /api/ingress/pull?token=…&id=…` → `{shared, commands}` (li marca `sent`) |
| Storico telemetria | `GET /api/iot/devices/:id/telemetry?key=&from=&to=&limit=` |
| Ultimi valori | `GET /api/iot/devices/:id/telemetry/latest` (include `metric`+`unit`) |
| Metriche canoniche | `GET /api/iot/metrics` (catalogo con etichetta IT + unità) |
| Mappa normalizzazione | `GET/PUT /api/iot/devices/:id/map` (`{rows:[{raw_key,metric,scale?,offset?}]}`) |
| Attributi | `GET/POST /api/iot/devices/:id/attributes` (`{scope, values}`) |
| Comando/RPC | `POST /api/iot/devices/:id/command` (`{method, params?}`) · `GET …/commands` |
| Soglie | `GET/POST /api/sensor-rules` (regole metric/op/threshold/etype) |
| Blueprint (lavori) | `GET /api/iot/blueprints` |
| Attivazione zero-touch | `POST /api/iot/activate` (`{blueprint, external_id, label?}`) → device+token+mappa+soglie+istruzioni |
| Webhook LoRaWAN/TTN | `POST /api/ingress/lorawan?token=…` (uplink con payload decodificato) |
| Broker MQTT nativo | TCP **1883** · PUBLISH su `yw/{tid}/{ext}/telemetry` · SUBSCRIBE a `yw/{tid}/{ext}/command` |
| Alert IoT (lista) | `GET /api/iot/alerts?state=&device=&severity=` · `GET /api/iot/alerts/summary` |
| Alert IoT (azioni) | `POST /api/iot/alerts/:id/ack` · `POST /api/iot/alerts/:id/close` |

## Alert IoT in vista separata (Fase 3)

Gli alert di **processo IoT** vivono in `app.iot_alerts`, **separati** dagli incidenti di sicurezza
del lavoratore (SOS/man down + escalation): un tenant puramente IoT li gestisce senza toccare il
mondo sicurezza. In console: menu **IoT Dashboard → Alert IoT** (notifica nativa live via SSE).

- **Tipi** (`kind`): `soglia` (regola sensori superata), `offline` (device muto oltre la soglia),
  `ripristino` (device tornato online).
- **Gravità** (`severity`): `critical` (`*_critico`, `perdita_alimentazione`), `warning` (batteria,
  offline, generico), `info` (ripristino/telemetria).
- **Ciclo di vita**: `aperto` → `preso_in_carico` (ack) → `chiuso`. Anti-spam: mentre un alert è
  aperto, nuove osservazioni ne incrementano le occorrenze senza duplicarlo né ri-notificare.
- **Watchdog offline**: per-device `offline_after_s` (impostato dal blueprint all'attivazione);
  il worker confronta `last_telemetry_at` con l'orologio controllabile (coerente, pilotabile in
  test). Alla ripresa della telemetria l'alert offline si chiude e nasce un `ripristino`.
- **Notifica nativa**: evento SSE `iot_alert` (nessun GoAlert: self-contained). Il record in
  `iot_alerts` è il registro persistente consultabile dalla vista.
- **Coesistenza**: con Lone Worker attivo la soglia alza **anche** l'incidente di sicurezza (bridge
  in `persist_telemetry`), così i due perimetri restano autonomi ma collaborano.

## Broker MQTT nativo (porta 1883)

Broker MQTT 3.1.1 **integrato** (nessun broker esterno). Il sensore si connette con l'**ingest
token come username** della CONNECT e:

- **pubblica** la telemetria JSON `{"values":{…}}` sul topic `yw/{tid}/{ext}/telemetry` → stessa
  normalizzazione + soglie + incidenti dell'HTTP (QoS 0 e 1 supportati; PUBACK sui QoS 1);
- **sottoscrive** `yw/{tid}/{ext}/command` per ricevere i comandi in coda (downlink): al momento
  della SUBSCRIBE il broker consegna i comandi `pending` del device e li marca `sent`.

**Isolamento multi-tenant**: il tenant è sempre quello del token; se il topic porta un `{tid}`,
deve combaciare, altrimenti la PUBLISH viene scartata. Un token vincolato a un device deve
combaciare con l'`{ext}` del topic (difesa in profondità).

Perimetro volutamente ristretto (no QoS 2, no messaggi *retained*/persistenza, no *will*): copre
l'ingest dei sensori e il downlink dei comandi. Disattivabile con `YW_MQTT_PORT=off`.

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
