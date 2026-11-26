# Ronde e punzonature — modulo «Ronde e vigilanza» (codice `giri`)

Ronde di controllo per guardie e manutentori. Un **percorso di ronda** è una sequenza ordinata di
**punti di controllo** (checkpoint). La guardia esegue una **ronda** sul percorso e **punzona** ogni
punto; alla chiusura si calcolano esito e anomalie. Il modulo è indipendente da interventi,
consuntivi, turni e Sicurezza Lavoratore, e viceversa.

Nomi: vedi [glossario](glossario.md). Nel codice e nelle API il percorso resta `patrol_routes` /
`/api/patrol-routes` e il modulo resta `giri`.

## Tipi di punto di controllo

| Tipo | Verifica della punzonatura |
|---|---|
| `qr` | il codice letto è uguale al `code` atteso |
| `nfc` | idem (contenuto del tag NFC) |
| `beacon` | idem (identificativo del beacon) |
| `gps` | la posizione della guardia è entro `radius_m` dal punto (`lat`/`lon`), distanza haversine |

Ogni punto può avere anche dei tempi (mig `0104`): `expected_min` (tempo atteso dalla tappa
precedente) e `max_min` (tempo massimo; oltre, con la tolleranza del percorso, è «tratta in
ritardo»). `dwell_max_min` (permanenza massima) è riservato e non usato: con una sola punzonatura per punto
non si sa quando la guardia se ne va.

## Integrità delle punzonature (mig `0134`, CA-2/CA-3)

Una punzonatura deve provare che la guardia **era sul posto**. Regole:

- **Il codice atteso non arriva mai alla guardia.** `GET /api/patrol-routes`, l'avvio ronda, la
  punzonatura e il dettaglio ronda restituiscono `code` solo a chi ha la capability `plan`.
- **Codici generati dal sistema.** Un punto QR o NFC creato senza codice riceve un codice
  casuale `YW-` + 16 caratteri (~80 bit): non si indovina e non si ricava dal nome. Va stampato
  sull'etichetta QR o scritto sul tag NFC. Se il pianificatore inserisce un codice di meno di 10
  caratteri la risposta contiene un `warning`. Per il beacon il codice è l'identificativo fisico e
  resta obbligatorio. In modifica, un codice vuoto mantiene quello esistente.
- **Metodo reale.** L'app dichiara come ha ottenuto il codice (`entry`): `scansione` (fotocamera,
  per QR), `nfc` (tag avvicinato), `beacon` (beacon rilevato), `manuale` (digitato). Un valore
  assente o che non corrisponde al tipo del punto vale `manuale`. Il digitato è salvato con metodo
  `qr_manuale`/`nfc_manuale`/`beacon_manuale`: è valido, ma a fine ronda genera l'anomalia
  **«Codice digitato a mano»**. Per i punti GPS `entry=gps`; le punzonature automatiche da
  dispositivo hanno `entry=auto` (vedi sotto).
- **Percorso «solo scansione».** Con `manual_code_allowed=false` sul percorso (casella *Consenti di
  digitare il codice*) il codice digitato è rifiutato: «inserimento manuale non ammesso su questo
  percorso».
- **Solo chi esegue la ronda punzona.** Il pianificatore vede e chiude le ronde altrui ma non può
  punzonare al posto di un'altra persona (403).
- **Posizione come prova.** Se il punto ha coordinate e l'app invia la posizione, la distanza
  è salvata (`distance_m`); oltre `radius_m` a fine ronda c'è l'anomalia **«Punzonato lontano dal
  punto»**. La punzonatura resta valida (il GPS al chiuso può essere impreciso).
- **Un tentativo fallito non cancella un OK** (CA-3). Ogni tentativo incrementa `attempts`;
  l'ultimo fallimento resta in `last_fail_at`/`last_fail_reason`. Il dettaglio ronda in console
  mostra modo di punzonatura, distanza, numero di tentativi e motivo dell'ultimo fallito.

Limite noto: un QR stampato fotografato una volta può essere riscansionato altrove. La distanza
registrata e l'anomalia «lontano dal punto» lo rendono visibile; per i punti critici usare NFC o
punti GPS.

### Punzonatura automatica da dispositivo

Se la guardia ha un dispositivo assegnato (badge GPS, app con posizione) e ha una ronda in corso,
ogni posizione ricevuta che cade nel raggio di un punto **GPS** non ancora punzonato registra la
punzonatura in automatico (`method=gps`, `entry=auto`, nota «auto da dispositivo»). Vale solo per i
punti GPS e solo con il modulo attivo.

## Modello dati

- `patrol_routes` (mig `0075`): `id, tenant_id, name, site_id?, window_minutes?, active, created_at`,
  UNIQUE(tenant_id, name). `window_minutes` = durata massima della ronda. Poi:
  `assigned_group` (mig `0076`), `mode` `piedi|mezzo` e `grace_min` tolleranza dei tempi (mig
  `0104`), `manual_code_allowed` default true (mig `0134`).
- `patrol_checkpoints` (mig `0075`): `id, tenant_id, route_id, name, position, kind, code?, lat?,
  lon?, radius_m` (default 50), `geofence_id?`; più `expected_min, max_min, dwell_max_min` (mig `0104`).
- `patrol_rounds` (mig `0075`): `id, tenant_id, route_id, user_id, started_at, ended_at?, status`
  con status `in_corso|completato|incompleto|abbandonata`; `closed_by` (mig `0141`, `auto` se
  chiusa dal sistema). Indice unico parziale `patrol_rounds_one_open` (una ronda in corso per
  percorso, mig `0141`).
- `patrol_punches` (mig `0075`, `0134`): `id, tenant_id, round_id, checkpoint_id, user_id,
  punched_at, method, lat?, lon?, ok, note, entry, attempts, last_fail_at?, last_fail_reason?,
  distance_m?`. UNIQUE(round_id, checkpoint_id): una punzonatura per punto; un esito OK non viene
  mai sovrascritto da un tentativo fallito.
- `patrol_anomalies` (mig `0104`): `round_id, checkpoint_id?, kind, detail, created_at`.
- `patrol_schedules` (mig `0105`): piani ricorrenti `route_id, days_mask` (7 cifre 0/1, lun→dom),
  `from_time, to_time, every_minutes, active`.
- `patrol_missed` (mig `0135`): una riga per orario mancato, UNIQUE(schedule_id, slot_at), con
  `incident_id`.

Tutte con RLS `tenant_isolation` e GRANT ad `app_rt`; FK in CASCADE verso il percorso. Il modulo
è in `modules_catalog` con codice `giri` (nome «Ronde e vigilanza» dalla mig `0104`).

## API

Configurazione (modulo `giri` + capability `plan`):

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/patrol-routes` | percorsi + punti (aperto anche alla guardia, senza codici; vedi «Assegnazione a gruppo») |
| POST | `/api/patrol-routes` | crea `{name, site_id?, window_minutes?, assigned_group?, mode?, grace_min?, manual_code_allowed?}` |
| POST | `/api/patrol-routes/:id` | aggiorna; `name`, `site_id` e `window_minutes` sono sostituiti (se omessi si azzerano), gli altri campi cambiano solo se presenti; `clear_group: true` toglie il gruppo |
| DELETE | `/api/patrol-routes/:id` | elimina percorso, punti e ronde (cascade) |
| POST | `/api/patrol-routes/:id/checkpoints` | aggiungi `{name, kind, code?, lat?, lon?, radius_m?, position?, expected_min?, max_min?}` → `{id, code, generated, warning?}` |
| POST | `/api/patrol-routes/:id/checkpoints/import` | **riusa** un punto di un altro percorso copiandolo in coda `{source_checkpoint_id}` (copia nome, tipo, codice, coordinate, raggio; non i tempi) |
| POST | `/api/patrol-checkpoints/:id` | aggiorna un punto |
| DELETE | `/api/patrol-checkpoints/:id` | elimina un punto |

Esecuzione (modulo `giri`; qualunque utente del tenant; il pianificatore vede tutte le ronde, gli
altri solo le proprie; punzona solo chi esegue la ronda):

| Metodo | Rotta | Azione |
|---|---|---|
| POST | `/api/patrol-routes/:id/rounds` | avvia una ronda (ritorna punti + progresso); 400 se il percorso non è attivo, 409 se c'è già una ronda in corso |
| GET | `/api/patrol-rounds` | elenco ronde (ultime 200) |
| GET | `/api/patrol-rounds/:id` | dettaglio + punzonature + anomalie + esito (`done`/`total`) |
| POST | `/api/patrol-rounds/:id/punch` | punzona `{checkpoint_id, code?, entry?, lat?, lon?, note?}` → `{ok, reason, round}` |
| POST | `/api/patrol-rounds/:id/close` | chiudi (chi la esegue o il pianificatore) → `completato` se tutti i punti sono OK entro la durata massima, altrimenti `incompleto`; ritorna `{status, in_window, anomalies, round}` |

Pianificazione e cruscotto:

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/patrol-schedules` | elenco dei piani (modulo `giri`) |
| POST | `/api/patrol-schedules` | crea `{route_id, days_mask?, from_time?, to_time?, every_minutes?}` (capability `plan`; default tutti i giorni, 22:00–06:00, ogni 60 min, minimo 5) |
| POST | `/api/patrol-schedules/:id` | sospende o riattiva `{active}`: riattivato, le ronde dovute ripartono da adesso (mig `0164`, capability `plan`) |
| DELETE | `/api/patrol-schedules/:id` | elimina un piano (capability `plan`; 404 se non esiste) |
| GET | `/api/patrol-dashboard` | cruscotto vigilanza (solo a chi supervisiona: `incidents.view`, `plan`, `devices.manage`, `devices.admin` o `iot.manage`) |
| GET/PUT | `/api/settings/timezone` | fuso orario dell'azienda `{timezone}` (modifica: `settings.manage`) |

Non c'è una rotta per modificare o sospendere un piano: si elimina e si ricrea.

Validazione: tipo fuori catalogo → 400; GPS senza coordinate → 400; beacon senza identificativo →
400 (QR/NFC senza codice: generato dal sistema); `days_mask` diverso da 7 cifre 0/1 → 400.

## Anomalie a fine ronda

Alla chiusura si ricalcolano le anomalie della ronda (`patrol_anomalies`):

| Codice | In console | Quando |
|---|---|---|
| `saltato` | Tappa saltata | punto senza punzonatura riuscita |
| `inserimento_manuale` | Codice digitato a mano | punzonatura con `entry=manuale` |
| `posizione_distante` | Punzonato lontano dal punto | `distance_m` oltre `radius_m` |
| `in_ritardo` | Tratta in ritardo | tempo dalla tappa precedente oltre `max_min` + `grace_min` |
| `non_completato` | Ronda non completata | ronda `incompleto` (punti mancanti o oltre la durata massima) |
| `abbandonata` | Ronda abbandonata (mai chiusa) | ronda chiusa dal sistema (vedi sotto) |

## UI

- **Console → Lavoro sul campo → Ronde**: il pianificatore crea i percorsi di ronda (sede, durata
  massima, tolleranza dei tempi, modalità a piedi/con mezzo, «consenti di digitare il codice»,
  gruppo di persone), gestisce i punti (QR/NFC/beacon con codice, GPS con lat/lon/raggio, tempo
  atteso e massimo; riuso di un punto da un altro percorso) e rivede le ronde eseguite con esito
  per punto, modo di punzonatura, tentativi e anomalie. Da qui percorsi e punti si creano e si
  eliminano; la modifica c'è solo via API.
- **Console → Lavoro sul campo → Cruscotto vigilanza**: contatori del giorno (svolte, mancanti,
  attese, anomalie), ronde in corso con avanzamento e, se la guardia ha un dispositivo con
  posizione, mappa; ronde dovute di oggi per percorso con l'ora locale; sezione *Pianificazione
  ronde* (solo pianificatore) per creare ed eliminare i piani.
- **App del lavoratore «Oggi» (`/worker/`)**: sezione **Ronde** (visibile solo se il modulo è
  attivo); la guardia avvia la ronda, punzona ogni punto, vede il progresso e chiude la ronda.
  - **QR**: scanner **nativo** con fotocamera (`BarcodeDetector`), overlay con mirino, timeout 30 s;
    pulsante «Annulla / digita» per l'inserimento manuale.
  - **NFC**: lettura del tag **nativa** (`NDEFReader`, Web NFC su Android/Chrome), timeout 15 s.
  - **beacon**: inserimento manuale del codice.
  - **GPS**: geolocalizzazione del browser.
  - Dove lo scanner non è supportato (browser o permessi), si ripiega sull'inserimento manuale del
    codice scritto sul punto, registrato come «codice digitato a mano»; sui percorsi «solo
    scansione» l'app non lo propone. Con QR/NFC l'app invia anche la posizione, se disponibile
    entro 4 s. La verifica resta sul server (confronto del codice, distanza GPS).

## Prove

`qa/patrols-e2e.sh` **14/14**: configurazione percorso e punti (QR+GPS), validazione, ACL di
configurazione, avvio ronda, punzonatura QR ok/ko, GPS dentro/fuori raggio, chiusura
completato/incompleto, esito 2/2, isolamento tenant, modulo spento → 403.

`qa/field-integrity-e2e.sh` **25/25** (mig `0134`): codice mai inviato alla guardia, codice
generato, avviso su codice corto, metodo reale, digitato = anomalia, percorso solo scansione,
tentativo fallito che non declassa l'OK, pianificatore che non punzona per altri, distanza dal
punto; più la parte ordini (CA-1/CA-7, vedi `campo-gruppi.md`).

## Ronde pianificate: fuso orario e ronda mancata (mig `0135`, CA-12/CA-16)

- **Fuso orario.** Gli orari dei piani (`from_time`/`to_time`, es. «22:00») sono nell'ora locale
  della **sede del percorso**; se la sede non ha un fuso proprio vale quello dell'**azienda**
  (`tenants.timezone`, default `Europe/Rome`).
  - Fuso dell'azienda: `GET/PUT /api/settings/timezone`; in console *Configurazione → Sedi e asset
    → Sedi → 🕒 fuso orario*.
  - Sede: campo `timezone` (vuoto = come l'azienda) nel modulo della sede.
  - Nuovo percorso: selettore **Sede** (serve per il fuso e per la sede dell'allarme).
  - `days_mask` vale per il giorno di inizio: una fascia 22:00→06:00 prosegue nel giorno dopo.
    «Dalle» = «alle» (es. 08:30–08:30) è **una sola ronda** a quell'ora, non una fascia di 24 ore.
  - La creazione del piano usa l'orologio del core, lo stesso del controllo delle ronde mancate:
    gli orari precedenti alla creazione non diventano mai ronde mancate.
  - Passaggio all'ora legale: un orario inesistente viene saltato.
- **Quando una ronda copre un orario.** Un orario è coperto se sul percorso è stata avviata una
  ronda tra metà intervallo prima dell'orario e la scadenza (orario + intervallo).
- **Ronda mancata.** Un controllo del worker (~ogni 30 s) guarda gli orari dei piani attivi, sui
  percorsi attivi, nei tenant non sospesi con il modulo attivo. Se la scadenza è passata e nessuna
  ronda copre l'orario, apre l'incidente **`ronda_mancata`** («Ronda mancata (orario pianificato
  non svolto)») attribuito alla sede del percorso. Escalation normale (gruppi di escalation,
  reperibili, rete di ultima istanza); **non** è un allarme salva-vita: la manutenzione lo sospende.
  - Un solo incidente per orario (`app.patrol_missed`, UNIQUE(schedule_id, slot_at)), anche con
    più istanze del worker.
  - Solo scadenze delle ultime 6 ore e mai orari precedenti alla creazione del piano: creare un
    piano non genera allarmi arretrati.
- **Cruscotto vigilanza.** Orari di oggi nel fuso del percorso; ogni orario mostra l'ora locale ed
  è *svolta*, *mancata* (scadenza passata senza ronda) o *in attesa*. L'orario con l'incidente
  aperto ha ⚠ e apre l'allarme. Le anomalie del giorno si contano dalla mezzanotte nel fuso
  dell'azienda.
- `qa/patrol-missed-e2e.sh` **13/13**.

## Ronde e presenze dimenticate (CA-17, mig `0141`)

- **Una ronda in corso per percorso**: avviare una seconda ronda sullo stesso percorso mentre una
  è in corso dà 409 con chi l'ha avviata e a che ora (anche indice unico parziale, creato se i dati
  lo permettono).
- **Ronda abbandonata**: una ronda in corso oltre *durata massima + tolleranza + 60 min* (8 ore se
  il percorso non ha durata massima) viene chiusa dal worker (~ogni minuto) come **abbandonata**
  (`closed_by='auto'`) con anomalia «Ronda abbandonata (mai chiusa)»; il percorso torna avviabile.
  Le ronde usano l'orologio del database (`now()` SQL), come avvio e punzonature.
- **Presenza di cantiere dimenticata**: aperta da oltre 16 ore → chiusa a **durata zero** e
  marcata *da verificare* (`auto_closed`): niente ore o costi inventati; la scheda commessa la
  segnala. Ingresso e uscita in cantiere usano l'orologio unico (`st.clock`).
- e2e `qa/stale-open-e2e.sh` **11/11**.

## Ronde nel registro delle ore (CA-4, mig `0158`)

Le ronde chiuse entrano nel registro unico delle ore (`app.work_hours`, pagina *Ore lavorate*)
come ore di servizio **non fatturabili**. Le ronde chiuse dal sistema (`closed_by='auto'`) non
contano. Precedenza in caso di sovrapposizione: intervento › ronda › presenza › turno. Le ronde
non diventano righe di consuntivo. Dettaglio: [consuntivi-correttezza](consuntivi-correttezza.md).

## Perimetro

Ronde = **Lavoro sul campo**. Dal 24/11/2026 (decisione PO, CA-12) il modulo produce un solo tipo
di allarme, la **ronda mancata**, e per questo porta con sé **Centrale ed escalation** anche senza
Sicurezza Lavoratore (`ESCALATION_MODS` e capability `incidents.*`/`escalation.manage` includono
`giri`). Niente SOS, check-in o dispositivi di sicurezza. Riusa le sedi (facoltative) e i
dispositivi con posizione (punzonatura automatica GPS, mappa del cruscotto); indipendente dagli
altri moduli campo.

**Assegnazione a gruppo (Tappa 3, mig `0076`):** un percorso può essere assegnato a un **gruppo di
persone** (`assigned_group`). In tal caso solo i membri (o il pianificatore) lo vedono e possono
avviare la ronda; i percorsi senza gruppo restano aperti a tutti. Dettaglio: `campo-gruppi.md`.

Prossimo possibile: modifica dei piani, finestra per fascia
oraria o turno.
