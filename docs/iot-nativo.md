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
- **Monitoraggio flotta** (Fase 4, vedi sotto): stato online/offline, ultimi valori canonici,
  tendenza (sparkline) e alert aperti per dispositivo, in un colpo d'occhio.
- **Catalogo sensori reali** (Fase 5, vedi sotto): archetipi di sensori industriali (CO₂, temperatura,
  livello, energia, porta, conteggio…) agganciati ai blueprint; si attiva "per modello" e il lavoro
  si deriva da sé.
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
| Blueprint (lavori) | `GET /api/iot/blueprints` (ogni blueprint include i `models` reali consigliati) |
| Catalogo modelli | `GET /api/catalog/devices?scope=iot` (o `safety`) — include `blueprint` + `capabilities`; `scope=iot` ritorna solo sensori |
| Attivazione zero-touch | `POST /api/iot/activate` (`{blueprint? | model_code?, external_id, label?}`) → device+token+mappa+soglie+istruzioni |
| Webhook LoRaWAN/TTN | `POST /api/ingress/lorawan?token=…` (uplink con payload decodificato) |
| Broker MQTT nativo | TCP **1883** · PUBLISH su `yw/{tid}/{ext}/telemetry` · SUBSCRIBE a `yw/{tid}/{ext}/command` |
| Alert IoT (lista) | `GET /api/iot/alerts?state=&device=&severity=` · `GET /api/iot/alerts/summary` |
| Alert IoT (azioni) | `POST /api/iot/alerts/:id/ack` · `POST /api/iot/alerts/:id/close` |
| Monitoraggio flotta | `GET /api/iot/monitor` (stato online/offline + ultimi valori + trend + alert) |

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
- **Notifica nativa**: evento SSE `iot_alert` (nessun YWAlert: self-contained). Il record in
  `iot_alerts` è il registro persistente consultabile dalla vista.
- **Coesistenza**: con Lone Worker attivo la soglia alza **anche** l'incidente di sicurezza (bridge
  in `persist_telemetry`), così i due perimetri restano autonomi ma collaborano.

## Monitoraggio IoT (Fase 4)

`GET /api/iot/monitor` dà l'overview della flotta IoT in una chiamata; in console: menu
**IoT Dashboard → Monitoraggio IoT**.

- **Stato online/offline** per device: `online` = `last_telemetry_at` entro `offline_after_s`
  (o un default di 3600 s se il watchdog non è impostato — segnalato come `has_watchdog:false`).
  Orologio controllabile (coerente col watchdog).
- **Ultimi valori canonici**: una riga per metrica (etichetta + unità dal catalogo `iot_metrics`),
  ordinate per categoria.
- **Tendenza**: `spark` = ultimi 24 punti numerici della metrica primaria (sparkline in console).
- **Alert aperti** per device + **riepilogo** flotta (totali / online / offline / senza watchdog /
  alert aperti).
- Aggiornamento **live** via SSE (telemetria e alert).

## Catalogo sensori reali (Fase 5)

Il catalogo `device_models` non è più solo tracker GPS: include **archetipi di sensori industriali**
(category `sensore`) con protocollo IoT (LoRaWAN / MQTT / HTTP), `capabilities` = **metriche
canoniche** misurate, e una colonna **`blueprint`** che li aggancia al profilo-lavoro.

- Esempi: Elsys ERS CO2 / Milesight AM103 → `air-quality`; Dragino LHT65 / Elsys ERS → `cold-chain`;
  Milesight EM500-UDL / Dragino LDDS75 → `tank-level`; Milesight WS301 / Dragino LDS02 →
  `door-contact`; Shelly EM / Milesight CT101 → `energy-meter`; Milesight VS121 → `people-count`;
  più i generici MQTT/HTTP/LoRaWAN (senza blueprint, flessibili).
- **Attivazione per modello**: `POST /api/iot/activate {model_code, external_id}` **deriva il
  blueprint** dal modello (se non si passa `blueprint`), e configura device + normalizzazione +
  soglie + watchdog da sé. "Quale sensore per quale lavoro; il resto si sistema." Un modello
  generico senza blueprint richiede un `blueprint` esplicito (errore onesto 400 altrimenti).
- `GET /api/iot/blueprints` restituisce per ogni lavoro l'elenco dei **sensori consigliati**
  (`models`), così il picker "Attiva per un lavoro" propone l'hardware giusto. Il device salva
  `model_code` (visibile in catalogo/monitoraggio).

### Separazione netta dei cataloghi (per perimetro)

Il catalogo `device_models` è unico ma **segmentato per perimetro** via il parametro `scope`:

- `scope=iot` → **solo** sensori IoT (`category='sensore'`): è ciò che vede il percorso
  "Attiva per un lavoro".
- `scope=safety` → tracker di **Sicurezza Lavoratore / Lavoro sul campo**
  (`personale/smartphone/veicolo/asset`): è ciò che vede "Aggiungi dispositivo (guidato)".
- Il bucket `altro` (2174 modelli GPS importati da Traccar) **non compare in elenco**: emerge
  **solo** quando si digita una ricerca testuale, così il catalogo curato non viene annegato.
  `GET /api/catalog/facets?scope=…` restituisce produttori/scopi coerenti col perimetro.

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
