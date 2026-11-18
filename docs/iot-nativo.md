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
- **Soglie → incidenti**: le regole sensori (`sensor_rules`) valutate sulla telemetria creano
  incidenti nativi (centrale + escalation con Lone Worker attivo).
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
