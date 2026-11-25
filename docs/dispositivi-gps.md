# Dispositivi GPS e badge (modulo `gps`)

Il modulo **Localizzazione GPS e badge** riceve dati da tracker personali, badge e smartphone e li
trasforma in posizioni e allarmi salva-vita. Tutta la decodifica è **nativa**: YouWorkforce non
dipende da server esterni.

**Attivazione e autonomia.** Il modulo si attiva dal superadmin sul singolo tenant (mig `0129`: prima
la riga di catalogo ereditava "non disponibile" dal vecchio connettore e il modulo non si accendeva).
È **autonomo**: non richiede *Lone Worker*. Un tenant con solo GPS (ronde, badge con SOS e caduta
propri, es. GH-5200) vede e gestisce in **Centrale allarmi** gli incidenti dei suoi badge (SOS, uomo
a terra, perdita di contatto) e configura escalation e reperibilità; i **check-in** restano di Lone
Worker e la Centrale non li mostra. Con entrambi i moduli attivi i due perimetri si sommano.

| Canale | Per chi | Come si identifica il dispositivo |
|---|---|---|
| **Teltonika TCP** (Codec8 / Codec8 Extended), porta `5027` | GH-5200, TMT250, FMB… | IMEI censito |
| **OsmAnd HTTP** (`GET/POST /api/ingress/osmand`) | smartphone, tracker compatibili | chiave di collegamento legata al dispositivo |
| **JSON HTTP** (`POST /api/ingress/gps`, `/api/ingress/gps/position`) | gateway e tracker che parlano HTTP | chiave di collegamento |

## Teltonika TCP

1. **Handshake IMEI**: il device apre il TCP e invia `[u16 lunghezza][IMEI ASCII]`. Il server
   risponde **`0x01`** (accetta) se l'IMEI corrisponde all'`external_id` di **un solo device GPS**
   (`source='gps'`) **in tutto il sistema**, su un tenant con **almeno un modulo dispositivi attivo**;
   altrimenti **`0x00`** (rifiuta) e chiude. L'identità è l'IMEI (il protocollo non porta token): si
   accettano solo IMEI noti e **non contesi**. A **ogni pacchetto** il bridge ricontrolla la
   titolarità: se il device è stato cancellato, spostato in un altro tenant o l'IMEI è diventato
   conteso, la connessione si chiude senza ACK e il badge, riconnettendosi, viene associato al
   titolare attuale. Un IMEI censito in più tenant viene **rifiutato**
   (log `identità ambigua`): non si associa mai in automatico a uno dei tenant, perché posizioni e
   SOS finirebbero al tenant sbagliato. Dalla migrazione 0127 il DB impedisce di registrare un IMEI
   GPS già presente in un altro tenant (errore "identificativo dispositivo GPS già registrato" →
   HTTP 409). Dalla 0128 la titolarità è la tabella `app.gps_identity` (chiave primaria
   sull'IMEI): l'unicità regge anche con scritture concorrenti a qualunque livello di isolamento.
   Cancellare il device libera l'IMEI; eventuali duplicati preesistenti sono marcati `contested`
   e restano rifiutati dal bridge finché un operatore non elimina il doppione sbagliato.
   Controllo in sola lettura: `qa/sql/check-imei-contesi.sql` (indica anche se il badge protegge
   un lavoratore).
2. **Pacchetti AVL**: `[preambolo 4B=0][lunghezza dati 4B][codec 1B][n record 1B] … [n record 1B][CRC16 4B]`.
   Supportati **Codec8 (`0x08`)** e **Codec8 Extended (`0x8E`)**. Per ogni record si decodificano
   timestamp (ms), GPS (lon/lat int32 /1e7, altitudine, angolo, satelliti, velocità) ed elementi IO
   (id evento, set a 1/2/4/8 byte; in 8E id e contatori a 2 byte + sezione NX variabile).
3. **ACK**: il server risponde con `u32` = numero di record accettati (come da specifica Teltonika).

Configurazione: `YW_TELTONIKA_PORT` (default **5027**; `off`/`0` disabilita). Il listener parte col
core e logga `teltonika edge TCP in ascolto su 0.0.0.0:<porta>`. Sul tracker (Teltonika
Configurator o SMS) si imposta come server l'indirizzo di YouWorkforce e la porta TCP.

### Badge rifiutati e verifica del collegamento (mig `0136`)

Un badge rifiutato riprova da solo all'infinito e prima nessuno lo sapeva. Ora ogni rifiuto
all'handshake è registrato in `app.device_connect_attempts` (una riga per IMEI, con contatore,
primo/ultimo tentativo e indirizzo di provenienza). Motivi:

| motivo | significato | cosa fare |
|---|---|---|
| `sconosciuto` | IMEI non censito in nessuna azienda | censire il badge con l'IMEI esatto |
| `conteso` | IMEI censito in più aziende (dato storico) | eliminare il doppione sbagliato |
| `modulo_spento` | azienda senza modulo dispositivi attivo | attivare il modulo `gps` |

- **Superadmin** (*Sistema → Dispositivi rifiutati*, `GET /api/superadmin/device-attempts`):
  elenco con motivo leggibile e aziende in cui l'IMEI risulta censito.
- **Admin del tenant**: nella scheda del badge, riquadro **Collegamento**, vede il rifiuto **solo dei
  propri IMEI** (mai quelli altrui) con motivo e indicazione.
- Quando l'IMEI viene accettato la riga sparisce. Righe ferme da 30 giorni vengono cancellate dal
  worker; la tabella è limitata a ~2000 righe (oltre si aggiornano solo quelle presenti, così un
  flusso di IMEI casuali non la gonfia). Tabella di sistema senza tenant: la legge solo il
  superadmin.
- **Collegato adesso**: `GET /api/devices` restituisce `connected_since` (connessione TCP aperta
  in questo momento, da quando) e `rejected` per i badge GPS. Il riquadro **Collegamento** della
  scheda mostra *Collegato adesso / Non collegato (ultimo contatto) / Mai collegato / Rifiutato* con
  il pulsante **verifica ora**: utile durante la prima installazione, senza aspettare un fix GPS.

### Comandi al dispositivo (codec12)

Sulla connessione aperta YouWorkforce può **inviare comandi** al device (codec12, type `0x05`) e
attendere la risposta (type `0x06`, max 5 s). Frame con CRC-16/IBM.

- **Comandi rapidi** (`GET /api/devices/:id/command-types`, `POST /api/devices/:id/command {type}`):
  *Chiedi la posizione adesso* (`getgps`), *Stato* (`getstatus`), *Informazioni di rete e SIM*
  (`getinfo`), *Versione firmware* (`getver`), *Riavvia* (`cpureset`). L'esito finisce nello
  **storico comandi** (`GET /api/devices/:id/commands`, `status_code` 200 inviato / 502 non
  inviato perché il device non è collegato).
- **Comando libero** per tecnici: `POST /api/devices/:id/gprs-command {command}` oppure il tipo
  `personalizzato` dei comandi rapidi. Richiede **Amministrazione dispositivi** (`devices.admin`):
  un comando libero può riconfigurare il device (server, APN, parametri).
- **RPC IoT** (`POST /api/iot/devices/:id/command`): verso un device GPS il metodo è consegnato come
  comando GPRS grezzo, quindi richiede anch'esso `devices.admin`.
- **Isolamento**: ogni connessione è registrata con la chiave **(tenant, IMEI)** e il device
  risolti all'handshake; un invio verso un device diverso (ricreato o spostato) non la usa. Tutti i percorsi di invio partono solo verso la connessione del **proprio**
  tenant; un altro tenant con lo stesso IMEI riceve "device non connesso". I comandi sulla stessa
  connessione sono serializzati (la risposta va sempre a chi ha inviato il comando) e una
  riconnessione sostituisce la precedente senza essere cancellata dalla chiusura di quella vecchia.
  Valgono solo per device GPS (`400` altrimenti) e con almeno un modulo dispositivi attivo.

## Ingest e allarmi

Ogni record, da qualsiasi canale, entra nella stessa pipeline:

- **Ultimo contatto** → `devices.last_seen_at`, aggiornato a **ogni pacchetto autenticato**, anche
  senza fix GPS (al chiuso il device resta "vivo"): Teltonika (handshake e ogni pacchetto), OsmAnd
  (anche solo batteria o allarme, senza `lat/lon`), JSON HTTP.
- **Moduli**: i canali di ingest (Teltonika, OsmAnd, JSON `/api/ingress/gps` e `/gps/position`)
  accettano dati se il tenant ha **almeno un modulo dispositivi** attivo (`loneworker`, `gps` o
  `iot`): un tenant con Lavoratore isolato + GPS e senza IoT riceve posizioni e SOS.
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
- **IO 240** (movimento) → telemetria e segnale di movimento per il rilevamento nativo dell'immobilità (vedi sotto).
- **GPS** → posizione del lavoratore su mappa e piantina.

`gps_alarm_etype` accetta sia i nomi dei protocolli (`fallDown`, `accident`) sia l'etype già risolto
(`man_down`, `immobilita`): la semantica salva-vita non degrada mai a `device_alarm`.

**Ritrasmissioni e allarmi ripetuti (SL-4).** Se non riceve l'ACK il badge ritrasmette lo stesso
record AVL: stesso IMEI, orario del record e allarme sono lo **stesso evento** e non aprono un secondo
incidente. Più pressioni del SOS (o più segnali di caduta) mentre l'incidente dello stesso badge e dello
stesso tipo è **ancora aperto** si annotano sulla sua cronologia («segnalazione ripetuta») invece di
aprire un nuovo incidente: l'escalation prosegue senza raffiche di avvisi. Dopo la chiusura, un nuovo
allarme apre un nuovo incidente. Vale anche per OsmAnd quando la richiesta porta l'orario del
dispositivo. e2e: `qa/telt-retransmit-e2e.sh`.

### Batteria sotto soglia su ogni record (SL-17, 25/11/2026)

Prima la soglia `battery_min` valeva solo per gli allarmi espliciti del dispositivo. I normali
record di posizione di Teltonika e OsmAnd non la controllavano: un badge poteva scaricarsi senza
alcun avviso. Ora ogni record con la batteria viene valutato. Si segnala **una volta per
discesa**, quando il valore passa da sopra (o sconosciuto) a sotto la soglia. Con la policy
`battery_as` (off / warning / critico) si sceglie tra evento e allarme con escalation. L'ultimo
valore letto viene sempre salvato, anche se il badge è fermo e la posizione non cambia. e2e
`qa/hardening-audit-devices-e2e.sh`.

### Profili di protezione: trigger «geofence» (SL-18)

Il trigger «geofence» del profilo era legato a un tipo di allarme inesistente e non aveva mai
effetto. Ora vale per gli allarmi di area reali (ingresso/uscita geofence e sito). La migrazione
`0145` lo accende nei profili esistenti e nei nuovi: comportamento invariato, gli allarmi di area
continuano ad arrivare. Spegnerlo in un profilo ora sopprime davvero quegli allarmi per i suoi
lavoratori.

### Silenzio del dispositivo → Perdita di contatto

Un badge **in servizio** che non trasmette da più della soglia apre un incidente
critico **`perdita_contatto`** (spento, scarico, senza copertura, manomesso: in ogni caso il
lavoratore non è più protetto). Lo controlla il worker ogni ~10 s (`worker::gps_silence_watchdog`).

- Soglia: `silence_after_s` dello strumento *gps / Allarmi e posizione dei dispositivi* (default
  **600 s**, `0` = disattivo). Si imposta da *Collegamento dispositivi → Soglie e allarmi*.
- **Un silenzio = un incidente**: l'identificativo è legato all'ultimo contatto, quindi lo stesso
  silenzio non si duplica e uno nuovo, dopo una ripresa, apre un nuovo incidente.
- **Stato di servizio** (`devices.service_state`, mig 0128), visibile nella scheda del dispositivo:
  | Stato | Significato | Watchdog |
  |---|---|---|
  | `in_servizio` | il badge protegge il lavoratore assegnato | attivo |
  | `in_attesa` | censito/assegnato, non ancora messo in servizio | nessun allarme |
  | `fuori_servizio` | ritirato: guasto, magazzino, disabilitato | nessun allarme |
- **Cosa avvia il conteggio**: la **messa in servizio** (pulsante *Metti in servizio* nella scheda,
  `POST /api/devices/:id/service {state}`, permesso `devices.manage`, richiede lavoratore assegnato
  e device abilitato). L'**assegnazione da sola non** mette in servizio. Il **passaggio di
  consegne** di un badge in servizio a un altro lavoratore **riavvia** il conteggio. Senza
  lavoratore il badge torna `in_attesa`; disabilitato passa `fuori_servizio` (trigger DB);
  rimettere in servizio è sempre un'azione esplicita.
- Il silenzio si misura da **GREATEST(ultimo contatto, messa in servizio/consegna)**
  (`last_seen_at`, `silence_armed_at`): un badge al chiuso che invia solo heartbeat non è
  silenzioso; un badge **messo in servizio che non si è mai collegato** fa scattare l'allarme alla
  soglia (l'incidente riporta `never_seen: true`). L'incidente riporta anche le età di ultimo
  contatto e ultima posizione.
- **Upgrade 0128**: chi era già monitorato (assegnato, abilitato, con almeno un contatto) resta
  `in_servizio` **con l'armamento di 0127 invariato**; un badge assegnato mai collegato diventa
  `in_attesa` (va messo in servizio); disabilitato `fuori_servizio`; non assegnato `in_attesa`.
  Prima del deploy `qa/sql/check-badge-0128.sql` (sola lettura) elenca per ogni badge lo stato
  dopo la 0128 e l'azione per il responsabile operativo; collaudo: `docs/collaudo-staging-gps.md`.

#### Turno, chiusura automatica e app (Q4, mig `0137`, decisione PO 24/11/2026)

- **Sempre, oppure solo durante il turno (scelta del tenant).** Di default il silenzio di un badge
  in servizio è sorvegliato **sempre** (decisione PO 25/11/2026 sera, mig `0150`: prima il
  default era «solo in turno» e chi non usava i turni non riceveva più allarmi di silenzio).
  Con `silence_shift_only` = `si` (strumento *gps / alarm_map*) lo si sorveglia solo mentre il
  lavoratore assegnato è **in turno**: turno aperto nell'app (`app.shifts` in corso) **oppure**
  turno pianificato che copre l'istante attuale (`app.shift_plans`, non annullato). I tenant che
  avevano il vecchio default sono tornati a `no`. Il conteggio parte da **GREATEST(ultimo contatto, messa in
  servizio, inizio turno)**: a inizio turno un badge spento fa scattare l'allarme alla soglia.
  La scheda del badge mostra **«fuori turno · silenzio non sorvegliato»** (`in_shift`,
  `silence_shift_only` in `GET /api/devices`).
  - Attenzione: con `si` un'azienda che non usa né i turni dell'app né la pianificazione **non
    riceve allarmi di silenzio**.
  - Il turno nell'app si apre con il modulo **Gestione turni** oppure **Lone Worker** (prima solo
    Gestione turni).
- **Chiusura automatica.** Quando il dispositivo torna in linea (qualunque pacchetto del badge, o
  un segno di vita dell'app) gli incidenti `perdita_contatto` aperti dal watchdog per quel
  dispositivo si chiudono da soli: esito *«rientrato: il dispositivo è di nuovo in linea»*, voce in
  timeline `by: sistema, auto: true`, audit `incident_auto_close`, avviso di chiusura a chi era
  coinvolto nell'escalation (`api::silence_auto_close`). Il controllo scatta solo se il contatto
  precedente era più vecchio di 30 s (nessun costo per pacchetto).
- **App del lavoratore.** Durante il turno l'app invia un battito ogni minuto
  (`POST /api/worker/heartbeat`; anche ogni posizione conta come segno di vita). Se tace oltre
  `app_silence_after_s` (strumento *loneworker / Silenzio dell'app*, default **900 s**, `0` =
  disattivo) si apre `perdita_contatto` sul dispositivo app (`source='pwa'`, correlazione
  `app-silence-…`); si chiude al battito successivo. Fuori turno l'app non è sorvegliata. Chi ha
  un **badge in servizio** è sorvegliato dal badge: l'app non genera un secondo allarme. L'app
  mostra durante il turno l'avviso «tienila aperta e il telefono acceso».
  - Limite: un'app web su telefono bloccato o in background può essere sospesa dal sistema
    operativo e smettere di inviare il battito; per questo la soglia dell'app è più lunga di
    quella del badge. Per chi lavora da solo in aree a rischio resta consigliato il badge.
- e2e `qa/silence-shift-e2e.sh` **17/17**.

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
- **Risultato parziale**: oltre **50.000 posizioni** nell'intervallo si calcola sulle prime 50.000
  (si legge una riga in più per accorgersene) e il risultato è dichiarato **parziale**: header
  `x-yw-report-partial: true` e `x-yw-report-covered-until`, campi `partial`/`covered_until` nel
  riepilogo, riga `# ATTENZIONE: risultato PARZIALE…` in fondo al CSV. Con `&envelope=1` la risposta
  JSON è sempre `{partial, covered_until, max_points, items|summary}` (usato dalla console, che
  mostra un avviso rosso); senza, la forma resta quella storica.

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
- `qa/device-attempts-e2e.sh` (15/15): IMEI non censito registrato con contatore, visibile solo al
  superadmin; accettazione che ripulisce il registro; «collegato adesso» a connessione aperta e non
  più dopo la chiusura; IMEI conteso (dato storico) e azienda senza modulo, motivo sul proprio badge.
- `qa/gh5200-lifesafety-e2e.sh` (8/8): IO 242 → `man_down`, IO 236 → `sos`, posizione, eliminazione
  completa del device.
- `qa/gps-native-e2e.sh` (23/23): nessun residuo dei vecchi connettori, ingest JSON di posizioni,
  report riepilogo/soste/tragitti/CSV, policy eventi, comandi nativi, watchdog del silenzio
  (un solo incidente, nessun allarme per device non assegnati).
- `qa/gps-isolation-e2e.sh` (36/36): badge simulato sul bridge; SOS solo nel tenant giusto; IMEI
  di un altro tenant non registrabile; con un duplicato storico nessuno dei tre percorsi di invio
  del tenant B raggiunge il socket di A, mentre A li consegna tutti; IMEI conteso rifiutato; testo
  libero/RPC senza `devices.admin` → 403; ingest JSON con iot spento; heartbeat OsmAnd senza fix vs
  età della posizione; armamento all'assegnazione; config strumento; report parziale.
- `qa/gps-service-watchdog-e2e.sh` (25/25): con orologio controllato, verifiche 5 s prima e 5 s
  dopo un intero `silence_after_s`: badge monitorato (e continuità dopo un nuovo contatto), in
  attesa e fuori servizio (visibili, nessun incidente), messo in servizio mai collegato (allarme),
  assegnazione senza messa in servizio, passaggio di consegne, validazioni.
- `qa/gps-isolation-e2e.sh` include titolarità corrente dopo spostamento tra tenant, riassegnazione
  e device ricreato (simulatore `qa/lib/telt_sim.py`).
- `qa/upgrade-0127-check.sh` (28/28, fuori da run-all): upgrade da un DB a versione 126 (dump);
  confronta l'esito previsto da `qa/sql/check-escalation-0127.sql` con l'effetto reale e con le
  notifiche simulate arrivate al ricevitore (`YW_ESC_SINK_LOG`).
- `qa/upgrade-0128-check.sh` (19/19, fuori da run-all): upgrade da un DB a versione 127 con
  orologio controllato; classificazione dei badge, watchdog dall'upgrade, titolarità GPS con
  scritture concorrenti in READ COMMITTED e REPEATABLE READ.
- `qa/osmand-ingest-e2e.sh`, `qa/gps-battery-policy-e2e.sh`, unit test `track::tests` e
  `api::device_identity_tests`.

## Console

Nel dettaglio di un dispositivo GPS: riquadro **Collegamento** (collegato adesso, rifiutato e
perché, ultimo contatto, «verifica ora»), stato di collegamento (in linea / silenzioso da …, con la
**stessa soglia** `silence_after_s` del watchdog; "ultimo contatto … fa" se il controllo è spento), **comandi
rapidi** con storico, **percorsi e soste** degli ultimi 7 giorni (con export CSV), **telemetria IO**
dell'ultimo fix e **comando libero** per tecnici.

## Immobilità prolungata (rilevamento nativo, SL-2, mig 0133)

Si configura nel **profilo di protezione** (Sicurezza Lavoratore → Profili di protezione → sezione
«Rileva l'immobilità»). Tutti i parametri li sceglie chi configura:

| Parametro | Default | Significato |
|---|---|---|
| Rileva l'immobilità | spento | attiva il controllo per i lavoratori del profilo |
| Fermo per almeno | 5 minuti (1–120) | tempo senza movimento prima dell'allarme |
| Preavviso sull'app | 60 s (0–600, minore della soglia) | quanto prima dell'allarme compare «Stai bene?» |
| Spostamento minimo | 15 m (2–500) | uno spostamento più piccolo non conta come movimento |
| Solo durante il turno | sì | vale solo con un turno in corso (richiede il modulo Forza lavoro e turni) |

Come funziona: ogni posizione del badge o dell'app (anche quelle scartate perché troppo vicine) e il
segnale di movimento del badge (IO 240) aggiornano l'ultimo movimento reale. Se il lavoratore resta
fermo, a «soglia − preavviso» l'app mostra a tutto schermo **«Stai bene?»** con suono e vibrazione:
**«Sto bene»** fa ripartire il conteggio. Alla soglia si apre **un** incidente critico *Immobilità
prolungata* (salva-vita: escalation, rete di ultima istanza, mai sospeso dalla manutenzione); un nuovo
movimento riarma il controllo. Vale solo per badge **in servizio** o app che **stanno trasmettendo**:
un dispositivo silente è gestito dalla perdita di contatto. Sul badge non c'è preavviso.
e2e: `qa/immobility-e2e.sh`.

