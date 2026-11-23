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
  `alarm_map` Traccar; attribuzione temporale dall'orario del record). La mappa IO→allarme è
  risolta **per connessione** con questa precedenza: **override per-tenant** (`teltonika.io_alarm_map`)
  **>** **mappa del MODELLO** (`device_models.default_config.io_alarm_map`) **>** **default built-in**.
  Default built-in: `1→sos` (DIN1/panico), `242→man_down` (ManDown/FallDown dei tracker personali
  GH/TMT), `236→alarm`, `247→crash`, `246→tow`.

### Tracker personali lone-worker (GH-5200, TMT250) — percorso salva-vita

Il **GH-5200** è a catalogo come `teltonika-gh-5200`, categoria **personale**, con
`io_alarm_map` del modello `{236→sos, 242→man_down}`. All'onboarding (capabilities
`sos,fall,movement`) il Device Onboarding Kit crea in automatico il **routing degli allarmi**
(`sos`/`man_down`/`immobilita` → gruppo) e il **profilo di protezione** del lavoratore. In ingest:

- **IO 236** (pulsante SOS) → etype **`sos`** (CRITICAL → incidente + escalation nativa).
- **IO 242** (ManDown/FallDown) → etype **`man_down`** (uomo a terra / brusca caduta, CRITICAL).
- **IO 240** (Movement) → telemetria; immobilità derivabile (`stopped_as_immobilita`).
- **GPS** → posizione del lavoratore su mappa/piantina.

`traccar_alarm_etype` accetta sia i token Traccar (`fallDown`/`accident`) sia l'etype canonico già
risolto (`man_down`, `immobilita`), così la semantica salva-vita non degrada mai a `device_alarm`.
La mappa resta **configurabile**: l'admin può ridefinire `teltonika.io_alarm_map` per-tenant senza
toccare il decoder.

### Onboarding con scanner QR/barcode (console)

Nel wizard *Aggiungi dispositivo · 2/2* il campo IMEI ha **📷 Scansiona**: apre la camera e legge il
**QR** (codice device) o il **barcode** (IMEI universale) dall'etichetta — niente digitazione a mano.
Usa `BarcodeDetector` nativo (Chromium/Android) con **fallback ZXing**; estrae l'IMEI (15 cifre) e lo
inserisce nel campo. In contesti senza camera (es. webview) degrada all'input manuale, nessun blocco.

## Configurazione

- **Avvio**: variabile `YW_TELTONIKA_PORT` (default **5027**; `off`/`0` disabilita). Il listener
  parte col core e logga `teltonika edge TCP in ascolto su 0.0.0.0:<porta>`.
- **Device**: censire il tracker con **`external_id = IMEI`** e `source = traccar`, poi puntare il
  device (config APN/server Teltonika) a `host:5027`.

## Onboarding guidato (console)

Dopo l'onboarding (Dispositivi & App → *Aggiungi guidato* → Onboarding Kit) la console mostra una
**guida di collegamento specifica per protocollo**, non i token/URL grezzi:

- **Teltonika** (TCP, identità = IMEI): passi numerati con **Server dati = `host:5027` (TCP)** da
  impostare sul tracker (app Teltonika Configurator o SMS). **Nessun token** da copiare — il device si
  identifica con l'IMEI, già registrato → appena si collega i dati arrivano da soli.
- **Smartphone/OsmAnd**: indirizzo server pronto da incollare nell'app Traccar Client (contiene già
  la credenziale).
- Token di ingest e inoltro da un server Traccar esistente restano disponibili sotto **«Altre opzioni»**
  (avanzate), chiusi di default. L'endpoint `POST /api/devices/onboarding-kit` ritorna `server_host` +
  `teltonika_port` per costruire la guida.

## Perimetro e sicurezza

Decode-edge: il perimetro Traccar resta solo la decodifica. Nessun server Traccar richiesto.
Autenticazione = IMEI censito (nessun IMEI sconosciuto viene accettato); per deployment pubblici
usare rete/VPN o un reverse-proxy TCP con allowlist. Multi-record e multi-pacchetto per connessione
supportati; connessioni con IMEI non valido vengono chiuse subito.

## Prove

`qa/teltonika-e2e.sh` **8/8**: IMEI sconosciuto rifiutato (0x00), IMEI censito accettato (0x01) +
Codec8 con ACK record, posizione ingerita (ultima posizione aggiornata), **telemetria IO decodificata
in attributes** (movement/temp/raw io), Codec8 Extended con ACK, event IO mappato → incidente.

`qa/gh5200-lifesafety-e2e.sh` **8/8** (percorso salva-vita GH-5200): device onboarded col modello,
**IO 242 → incidente `man_down`**, **IO 236 → incidente `sos`** (via mappa modello, non `device_alarm`),
posizione GPS ingerita, device eliminabile del tutto. Regressione: teltonika 8/8, device-onboarding-kit 13/13.

## Console

Nel dettaglio dispositivo (Dispositivi & app → device con sorgente *traccar*): pannello
**«Telemetria IO (ultimo fix)»** (mostra gli attributi IO dell'ultima posizione: nomi noti + mappa
grezza) e **«Comando GPRS diretto (Teltonika)»** (campo comando → `POST .../gprs-command`, mostra la
risposta del device o l'errore se non connesso).

## Prossimo possibile

Onboarding kit (DOK) che espone host:porta TCP nel riepilogo device; comandi GPRS predefiniti
(bottoni) oltre al campo libero.
