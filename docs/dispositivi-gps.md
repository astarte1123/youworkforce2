# Dispositivi GPS e badge (modulo `gps`)

Il modulo **Localizzazione GPS e badge** riceve dati da tracker personali, badge e smartphone e li
trasforma in posizioni e allarmi salva-vita. Tutta la decodifica è **nativa**: YouWorkforce non
dipende da server esterni.

| Canale | Per chi | Come si identifica il dispositivo |
|---|---|---|
| **Teltonika TCP** (Codec8 / Codec8 Extended), porta `5027` | GH-5200, TMT250, FMB… | IMEI censito |
| **OsmAnd HTTP** (`GET/POST /api/ingress/osmand`) | smartphone, tracker compatibili | chiave di collegamento legata al dispositivo |
| **JSON HTTP** (`POST /api/ingress/gps`, `/api/ingress/gps/position`) | gateway e tracker che parlano HTTP | chiave di collegamento |

## Teltonika TCP

1. **Handshake IMEI**: il device apre il TCP e invia `[u16 lunghezza][IMEI ASCII]`. Il server
   risponde **`0x01`** (accetta) se l'IMEI corrisponde all'`external_id` di un **device censito** su
   un tenant con **almeno un modulo dispositivi attivo**; altrimenti **`0x00`** (rifiuta) e chiude.
   L'identità è l'IMEI (il protocollo non porta token): si accettano solo IMEI noti.
2. **Pacchetti AVL**: `[preambolo 4B=0][lunghezza dati 4B][codec 1B][n record 1B] … [n record 1B][CRC16 4B]`.
   Supportati **Codec8 (`0x08`)** e **Codec8 Extended (`0x8E`)**. Per ogni record si decodificano
   timestamp (ms), GPS (lon/lat int32 /1e7, altitudine, angolo, satelliti, velocità) ed elementi IO
   (id evento, set a 1/2/4/8 byte; in 8E id e contatori a 2 byte + sezione NX variabile).
3. **ACK**: il server risponde con `u32` = numero di record accettati (come da specifica Teltonika).

Configurazione: `YW_TELTONIKA_PORT` (default **5027**; `off`/`0` disabilita). Il listener parte col
core e logga `teltonika edge TCP in ascolto su 0.0.0.0:<porta>`. Sul tracker (Teltonika
Configurator o SMS) si imposta come server l'indirizzo di YouWorkforce e la porta TCP.

### Comandi al dispositivo (codec12)

Sulla connessione aperta YouWorkforce può **inviare comandi** al device (codec12, type `0x05`) e
attendere la risposta (type `0x06`, max 5 s). Frame con CRC-16/IBM.

- **Comandi rapidi** (`GET /api/devices/:id/command-types`, `POST /api/devices/:id/command {type}`):
  *Chiedi la posizione adesso* (`getgps`), *Stato* (`getstatus`), *Informazioni di rete e SIM*
  (`getinfo`), *Versione firmware* (`getver`), *Riavvia* (`cpureset`). L'esito finisce nello
  **storico comandi** (`GET /api/devices/:id/commands`, `status_code` 200 inviato / 502 non
  inviato perché il device non è collegato).
- **Comando libero** per tecnici: `POST /api/devices/:id/gprs-command {command}`.

## Ingest e allarmi

Ogni record, da qualsiasi canale, entra nella stessa pipeline:

- **Ultimo contatto** → `devices.last_seen_at`, aggiornato a **ogni pacchetto**, anche senza fix GPS
  (al chiuso il device resta "vivo").
- **Posizione** → `process_gps_position`: storico (`device_positions`), ultima posizione, presenza
  nelle aree (geofence), primo fix → attivo. Filtri anti-rumore per tenant: salto implausibile
  (`max_jump_kmh`) e spostamento minimo (`min_move_m`).
- **Batteria** → IO **113** (livello %), con policy per tenant (ignora / avviso / allarme critico).
- **Telemetria IO** → tutti gli IO del record in `attributes`: mappa grezza `io:{<id>:<val>}` + nomi
  noti (`ext_voltage_mv` 66, `battery_voltage_mv` 67, `gsm_signal` 21, `gnss_status` 69,
  `sleep_mode` 200, `movement` 240, `odometer_total_m` 16, `trip_odometer_m` 199, `din1/2/3`,
  `dout1/2`, `temp1/2/3_dc`).
- **Allarme** → se l'`event IO id` è mappato, si crea l'incidente con attribuzione temporale
  dall'orario del record. Precedenza della mappa IO→allarme: **override del tenant**
  (`teltonika.io_alarm_map`) **>** **mappa del modello** (`device_models.default_config.io_alarm_map`)
  **>** **default**: `1→sos` (DIN1/panico), `242→man_down`, `236→alarm`, `247→crash`, `246→tow`.

### Tracker personali lone-worker (GH-5200, TMT250) — percorso salva-vita

Il **GH-5200** è a catalogo come `teltonika-gh-5200`, categoria **personale**, con mappa del modello
`{236→sos, 242→man_down}`. All'onboarding (capacità `sos,fall,movement`) il Device Onboarding Kit
crea il **routing degli allarmi** (`sos` / `man_down` / `immobilita` → gruppo) e il **profilo di
protezione** del lavoratore.

- **IO 236** (pulsante SOS) → **`sos`** (critico → incidente + escalation).
- **IO 242** (caduta / uomo a terra) → **`man_down`** (critico).
- **IO 240** (movimento) → telemetria; l'immobilità prolungata può diventare allarme (`stopped_as_immobilita`).
- **GPS** → posizione del lavoratore su mappa e piantina.

`gps_alarm_etype` accetta sia i nomi dei protocolli (`fallDown`, `accident`) sia l'etype già risolto
(`man_down`, `immobilita`): la semantica salva-vita non degrada mai a `device_alarm`.

### Silenzio del dispositivo → Perdita di contatto

Un dispositivo **assegnato a un lavoratore** che non trasmette da più della soglia apre un incidente
critico **`perdita_contatto`** (spento, scarico, senza copertura, manomesso: in ogni caso il
lavoratore non è più protetto). Lo controlla il worker ogni ~10 s (`worker::gps_silence_watchdog`).

- Soglia: `silence_after_s` dello strumento *gps / Allarmi e posizione dei dispositivi* (default
  **600 s**, `0` = disattivo). Si imposta da *Collegamento dispositivi → Soglie e allarmi*.
- **Un silenzio = un incidente**: l'identificativo è legato all'ultimo contatto, quindi lo stesso
  silenzio non si duplica e uno nuovo, dopo una ripresa, apre un nuovo incidente.
- I dispositivi non assegnati (a magazzino) non generano allarmi di silenzio.

### Policy eventi

Per gli eventi diversi da SOS / caduta / immobilità / perdita di contatto (eccesso di velocità,
movimento non accompagnato, ingresso/uscita area, …) il tenant decide: **ignora**, **registra nel
diario**, **allarme con escalation** (`GET/POST /api/gps/event-policy`).

## Report percorsi e soste

Calcolati da YouWorkforce sullo storico posizioni (`core/src/track.rs`), uguali per tutti i canali:
`GET /api/devices/:id/report/{summary|trips|stops|route}?from=…&to=…[&csv=true]` (massimo 31 giorni).

- **Sosta**: il dispositivo resta entro 50 m per almeno 5 minuti.
- **Tragitto**: il movimento tra due soste, se supera 100 m.
- **Riepilogo**: distanza, tempo in movimento, velocità media e massima, numero di tragitti e soste.
- Le velocità si ricavano da distanza/tempo tra posizioni consecutive (il campo velocità dei device
  usa unità diverse a seconda del protocollo); i salti oltre 300 km/h sono scartati.

## Onboarding con scanner QR/barcode (console) + PWA

Nel wizard *Aggiungi dispositivo · 2/2* il campo IMEI ha **📷 Scansiona**: apre la camera e legge il
**QR** o il **barcode** (IMEI) dall'etichetta, senza digitazione a mano. Estrae l'IMEI (15/16 cifre)
e lo inserisce; se il codice non è un IMEI lo propone comunque con avviso (nessuno scarto silenzioso).

**Scanner v3** (pensato per webcam PC a fuoco fisso e codici piccoli su superfici lucide):

- motori: **jsQR** per i QR + **ZXing core** (`MultiFormatReader`+`HybridBinarizer`, `TRY_HARDER`)
  per i **barcode 1D**;
- **video visibile** nel DOM (un `<video>` staccato viene rallentato dal browser e si decodificava
  un'immagine nera); pipeline *video → area di lettura → canvas → decodifica*;
- **pre-elaborazione**: scala di grigi + aumento del contrasto + tentativo con colori invertiti;
- **zoom hardware** quando disponibile, altrimenti **zoom software**; slider 1–5× + **auto-zoom
  progressivo**; **tocca per mirare**; **torcia** e **messa a fuoco continua** se supportate;
  cattura **1920×1080**;
- **contatore tentativi a schermo** + log `[scan] engine=…`. Serve contesto sicuro
  (https/localhost) e un browser esterno, non la webview.

**PWA**: la console è installabile (`manifest.webmanifest` + `sw.js` **network-first**, le `/api`
non sono mai in cache). Dal telefono si installa sulla home e usa la camera posteriore per
l'onboarding sul campo.

## Onboarding guidato (console)

Dopo l'onboarding (Dispositivi & App → *Aggiungi guidato*) la console mostra una **guida di
collegamento specifica per protocollo**:

- **Teltonika**: passi numerati con **server = indirizzo di YouWorkforce, porta TCP 5027**. Nessuna
  chiave da copiare: il device si identifica con l'IMEI, già registrato.
- **Smartphone (OsmAnd)**: indirizzo pronto da incollare nell'app (contiene già la chiave).
- Chiave di collegamento e invio JSON HTTP restano disponibili sotto **«Altre opzioni»**.

## Sicurezza

Autenticazione = IMEI censito (Teltonika) o chiave di collegamento legata al dispositivo (HTTP): un
payload non può spacciarsi per un altro dispositivo e il tenant non deriva mai dal contenuto. Per
deployment pubblici usare rete/VPN o un reverse-proxy TCP con allowlist per la porta 5027.

## Prove

- `qa/teltonika-e2e.sh` (8/8): IMEI sconosciuto rifiutato, IMEI censito accettato + ACK, posizione e
  telemetria IO, Codec8 Extended, evento IO → incidente.
- `qa/gh5200-lifesafety-e2e.sh` (8/8): IO 242 → `man_down`, IO 236 → `sos`, posizione, eliminazione
  completa del device.
- `qa/gps-native-e2e.sh` (23/23): nessun residuo dei vecchi connettori, ingest JSON di posizioni,
  report riepilogo/soste/tragitti/CSV, policy eventi, comandi nativi, watchdog del silenzio
  (un solo incidente, nessun allarme per device non assegnati).
- `qa/osmand-ingest-e2e.sh`, `qa/gps-battery-policy-e2e.sh`, unit test `track::tests`.

## Console

Nel dettaglio di un dispositivo GPS: stato di collegamento (in linea / silenzioso da …), **comandi
rapidi** con storico, **percorsi e soste** degli ultimi 7 giorni (con export CSV), **telemetria IO**
dell'ultimo fix e **comando libero** per tecnici.
