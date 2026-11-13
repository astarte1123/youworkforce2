# Edge nativo Teltonika — TCP Codec8 / Codec8 Extended

Ricezione **diretta** dai tracker Teltonika (FMB/GH…) sul loro protocollo binario TCP, **senza
server Traccar** (decode-edge nativo). Completa i canali d'ingest già presenti (OsmAnd/HTTP).

## Protocollo

1. **Handshake IMEI**: il device apre il TCP e invia `[u16 lunghezza][IMEI ASCII]`. Il server
   risponde **`0x01`** (accetta) se l'IMEI corrisponde all'`external_id` di un **device censito** su
   un tenant con **almeno un modulo dispositivi attivo**; altrimenti **`0x00`** (rifiuta) e chiude.
   L'identità è l'IMEI (il protocollo non porta token): si accettano solo IMEI noti.
2. **Pacchetti AVL**: `[preambolo 4B=0][lunghezza dati 4B][codec 1B][n record 1B] … [n record 1B][CRC16 4B]`.
   Supportati **Codec8 (`0x08`)** e **Codec8 Extended (`0x8E`)**. Per ogni record si decodificano
   timestamp (ms), GPS (lon/lat int32 /1e7, altitudine, angolo, satelliti, velocità) ed elemento IO
   (id evento, set a 1/2/4/8 byte; in 8E id e contatori a 2 byte + sezione NX variabile).
3. **ACK**: il server risponde con `u32` = numero di record accettati (come da specifica Teltonika).

### Comandi GPRS (codec12)

Sul socket aperto il server può **inviare comandi** al device (codec12, type `0x05`): la connessione
viene registrata per IMEI, un writer task invia il frame comando e il reader intercetta la risposta
(codec12 type `0x06`). Endpoint **`POST /api/devices/:id/gprs-command {command}`** (tenant_admin):
risolve l'IMEI del device, invia il comando e **attende la risposta** (max 5s), che torna in
`{"ok":true,"response":"..."}`. Device non connesso → **502**. Frame con CRC-16/IBM.

## Ingest

Ogni record entra nella **pipeline nativa** (la stessa di OsmAnd/HTTP):

- **Posizione** → `process_traccar_position` (storico, ultima posizione, presenza geofence,
  primo-fix→attivo) per i device con `source='traccar'` (decode-edge).
- **Batteria** → IO **113** (livello %).
- **Telemetria IO** → tutti gli AVL IO del record finiscono in `attributes`: mappa grezza
  `io:{<id>:<val>}` + alcuni nomi noti Teltonika (`ext_voltage_mv` 66, `battery_voltage_mv` 67,
  `gsm_signal` 21, `gnss_status` 69, `sleep_mode` 200, `movement` 240, `odometer_total_m` 16,
  `trip_odometer_m` 199, `din1/2/3` 1/2/3, `dout1/2` 179/180, `temp1/2/3_dc` 72/73/74). Supporta
  set a 1/2/4/8 byte (Codec8) e id/contatori a 2 byte (Codec8E).
- **Allarme** → se l'`event IO id` è mappato, `persist_ingest` crea l'incidente (etype via
  `alarm_map` Traccar; attribuzione temporale dall'orario del record). Mappa di **default**
  built-in: `1→sos` (DIN1/panico), `236→alarm`, `247→crash`, `246→tow`; override per-tenant
  possibile via config `teltonika.io_alarm_map`.

## Configurazione

- **Avvio**: variabile `YW_TELTONIKA_PORT` (default **5027**; `off`/`0` disabilita). Il listener
  parte col core e logga `teltonika edge TCP in ascolto su 0.0.0.0:<porta>`.
- **Device**: censire il tracker con **`external_id = IMEI`** e `source = traccar`, poi puntare il
  device (config APN/server Teltonika) a `host:5027`.

## Perimetro e sicurezza

Decode-edge: il perimetro Traccar resta solo la decodifica. Nessun server Traccar richiesto.
Autenticazione = IMEI censito (nessun IMEI sconosciuto viene accettato); per deployment pubblici
usare rete/VPN o un reverse-proxy TCP con allowlist. Multi-record e multi-pacchetto per connessione
supportati; connessioni con IMEI non valido vengono chiuse subito.

## Prove

`qa/teltonika-e2e.sh` **7/7**: IMEI sconosciuto rifiutato (0x00), IMEI censito accettato (0x01) +
Codec8 con ACK record, posizione ingerita (ultima posizione aggiornata), **telemetria IO decodificata
in attributes** (movement/temp/raw io), Codec8 Extended con ACK, event IO mappato → incidente.
Regressione verde (loneworker 15, patrols 14, iot-mgmt 8).

## Console

Nel dettaglio dispositivo (Dispositivi & app → device con sorgente *traccar*): pannello
**«Telemetria IO (ultimo fix)»** (mostra gli attributi IO dell'ultima posizione: nomi noti + mappa
grezza) e **«Comando GPRS diretto (Teltonika)»** (campo comando → `POST .../gprs-command`, mostra la
risposta del device o l'errore se non connesso).

## Prossimo possibile

Onboarding kit (DOK) che espone host:porta TCP nel riepilogo device; comandi GPRS predefiniti
(bottoni) oltre al campo libero.
