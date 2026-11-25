# Ronde e punzonature — Campo Slice 2 (modulo «giri»)

Giri di controllo per guardie/manutentori: un **giro** (route) è una sequenza ordinata di
**checkpoint**; l'operatore esegue una **ronda** (round) e **punzona** (punch) ogni checkpoint; a
fine giro si calcola la **compliance**. Modulo **`giri`** indipendente: funziona senza
interventi/consuntivi/workforce/loneworker, e viceversa. Additivo e non distruttivo.

## Tipi di checkpoint

| Tipo | Verifica punzonatura |
|---|---|
| `qr` | il codice scansionato combacia con `code` atteso |
| `nfc` | idem (payload tag NFC) |
| `beacon` | idem (id beacon) |
| `gps` | la posizione dell'operatore è entro `radius_m` dal punto (`lat`/`lon`), via haversine |

## Integrità delle punzonature (mig `0134`, CA-2/CA-3)

Una punzonatura deve provare che la guardia **era sul posto**. Regole:

- **Il codice atteso non arriva mai alla guardia.** `GET /api/patrol-routes`, l'avvio ronda, la
  punzonatura e il dettaglio ronda restituiscono `code` solo a chi ha la capability `plan`.
- **Codici generati dal sistema.** Un checkpoint QR o NFC creato senza codice riceve un codice
  casuale `YW-` + 16 caratteri (~80 bit): non si indovina e non si ricava dal nome. Va stampato
  sull'etichetta QR o scritto sul tag NFC. Se il pianificatore inserisce un codice di meno di 10
  caratteri la risposta contiene un `warning`. Per il beacon il codice è l'identificativo fisico e
  resta obbligatorio. In modifica, un codice vuoto mantiene quello esistente.
- **Metodo reale.** L'app dichiara come ha ottenuto il codice (`entry`): `scansione` (fotocamera),
  `nfc` (tag avvicinato), `manuale` (digitato). Se manca vale `manuale`. Il digitato è salvato con
  metodo `qr_manuale`/`nfc_manuale`, è valido ma a fine ronda genera l'anomalia
  **«Codice digitato a mano»**. Le punzonature automatiche da dispositivo hanno `entry=auto`.
- **Giro «solo scansione».** Con `manual_code_allowed=false` sul giro (casella *Consenti di digitare
  il codice*) il codice digitato è rifiutato: «inserimento manuale non ammesso su questo giro».
- **Solo chi esegue la ronda punzona.** Il pianificatore vede e chiude le ronde altrui ma non può
  punzonare al posto di un'altra persona (403).
- **Posizione come prova.** Se il checkpoint ha coordinate e l'app invia la posizione, la distanza
  è salvata (`distance_m`); oltre `radius_m` a fine ronda c'è l'anomalia **«Punzonato lontano dal
  punto»**. La punzonatura resta valida (il GPS al chiuso può essere impreciso).
- **Un tentativo fallito non cancella un OK** (CA-3). Ogni tentativo incrementa `attempts`;
  l'ultimo fallimento resta in `last_fail_at`/`last_fail_reason`. Il dettaglio ronda in console
  mostra modo di punzonatura, distanza, numero di tentativi e motivo dell'ultimo fallito.

Limite noto: un QR stampato fotografato una volta può essere riscansionato altrove. La distanza
registrata e l'anomalia «lontano dal punto» lo rendono visibile; per i punti critici usare NFC o
checkpoint GPS.

## Modello dati (mig `0075_patrols.sql`)

- `patrol_routes(id, tenant_id, name, site_id?, window_minutes?, active, created_at)` —
  UNIQUE(tenant_id,name). `window_minutes` = finestra max per completare il giro.
- `patrol_checkpoints(id, tenant_id, route_id, name, position, kind, code?, lat?, lon?, radius_m, geofence_id?)`.
- `patrol_rounds(id, tenant_id, route_id, user_id, started_at, ended_at?, status)` —
  status `in_corso|completato|incompleto`.
- `patrol_punches(id, tenant_id, round_id, checkpoint_id, user_id, punched_at, method, lat?, lon?, ok, note,
  entry, attempts, last_fail_at?, last_fail_reason?, distance_m?)` — UNIQUE(round_id,checkpoint_id): una
  punzonatura per checkpoint; un esito OK non viene mai sovrascritto da un tentativo fallito (mig 0134).
- `patrol_routes.manual_code_allowed` (default true, mig 0134): ammette il codice digitato.

Tutte RLS `tenant_isolation` + GRANT app_rt; FK CASCADE. Il modulo è registrato in
`modules_catalog` (code `giri`).

## API

Configurazione (richiede modulo `giri` + capability `plan`/is_planner):

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/patrol-routes` | giri + checkpoint |
| POST | `/api/patrol-routes` | crea giro `{name, site_id?, window_minutes?, manual_code_allowed?}` |
| POST | `/api/patrol-routes/:id` | aggiorna `{name, site_id?, window_minutes?, active?, manual_code_allowed?}` |
| DELETE | `/api/patrol-routes/:id` | elimina (cascade) |
| POST | `/api/patrol-routes/:id/checkpoints` | aggiungi `{name, kind, code?/lat?/lon?/radius_m?}` → `{id, code, generated, warning?}` |
| POST | `/api/patrol-routes/:id/checkpoints/import` | **riusa** un checkpoint esistente copiandolo `{source_checkpoint_id}` |
| POST | `/api/patrol-checkpoints/:id` | aggiorna checkpoint |
| DELETE | `/api/patrol-checkpoints/:id` | elimina checkpoint |

Esecuzione (richiede modulo `giri`; qualunque utente del tenant; il pianificatore vede tutte le
ronde, gli altri solo le proprie; punzona solo chi esegue la ronda):

| Metodo | Rotta | Azione |
|---|---|---|
| POST | `/api/patrol-routes/:id/rounds` | avvia una ronda (ritorna checkpoint + progresso) |
| GET | `/api/patrol-rounds` | elenco ronde |
| GET | `/api/patrol-rounds/:id` | dettaglio + punzonature + compliance (`done`/`total`) |
| POST | `/api/patrol-rounds/:id/punch` | punzona `{checkpoint_id, code?, entry?, lat?, lon?, note?}` → `{ok, reason, round}` |
| POST | `/api/patrol-rounds/:id/close` | chiudi → `completato` se tutti i punti OK entro la finestra, altrimenti `incompleto` |

Validazione: tipo fuori catalogo → 400; gps senza coord → 400; beacon senza identificativo → 400
(QR/NFC senza codice: generato dal sistema).

## UI

- **Console → Lavoro sul campo → Ronde**: il pianificatore crea giri, gestisce i checkpoint
  (QR/NFC/beacon con codice, GPS con lat/lon/raggio) e rivede le ronde eseguite con esito per
  checkpoint e compliance.
- **PWA «Oggi» (`/worker/`)**: sezione **Ronde** (visibile solo se il modulo è attivo); l'operatore
  avvia la ronda, punzona ogni checkpoint, vede il progresso e chiude il giro.
  - **QR**: scanner **nativo** con fotocamera (`BarcodeDetector`) — overlay con mirino, timeout 30s;
    bottone «Annulla / digita» per l'inserimento manuale.
  - **NFC**: lettura tag **nativa** (`NDEFReader`, Web NFC su Android/Chrome).
  - **beacon**: inserimento codice manuale.
  - **GPS**: geolocalizzazione del browser.
  - Dove lo scanner non è supportato (browser/permessi), si ripiega sull'input manuale del codice
    scritto sul punto, registrato come «codice digitato a mano»; sui giri «solo scansione»
    l'app non lo propone. Con QR/NFC l'app invia anche la posizione se disponibile entro 4 s.
    La verifica resta lato server (match codice / distanza GPS).

## Prove

`qa/patrols-e2e.sh` **14/14**: config giro+checkpoint (qr+gps), validazione, ACL config, avvio
ronda, punch QR ok/ko, punch GPS dentro/fuori raggio, chiusura completato/incompleto, compliance
2/2, isolamento tenant, gating modulo off → 403. Regressione verde (field-slice1 8, custom-roles
16, loneworker 15).

`qa/field-integrity-e2e.sh` **25/25** (mig 0134): codice mai inviato alla guardia, codice
generato, avviso su codice corto, metodo reale, digitato = anomalia, giro solo scansione, tentativo
fallito che non declassa l'OK, pianificatore che non punzona per altri, distanza dal punto; più
la parte ordini (CA-1/CA-7, vedi `campo-gruppi.md`).

## Ronde pianificate: fuso orario e ronda mancata (mig `0135`, CA-12/CA-16)

- **Fuso orario.** Gli orari dei piani (`from_time`/`to_time`, es. «22:00») sono nell'ora locale
  della **sede del giro**; se la sede non ha un fuso proprio vale quello del **tenant** (default
  `Europe/Rome`). Prima erano letti in UTC (un'ora di differenza in inverno, due in estate).
  - `GET/PUT /api/settings/timezone` `{timezone}` (modifica: capability `settings.manage`); in
    console: *Utenti e sedi → Sedi → 🕒 fuso orario*.
  - Sede: campo `timezone` (vuoto = come il tenant) nel form sede.
  - Nuovo giro: selettore **Sede** (serve per il fuso e per la sede dell'allarme).
  - Passaggio all'ora legale: un orario inesistente viene saltato.
- **Ronda mancata.** Un controllo del worker (~ogni 30 s) guarda gli slot dei piani attivi: se la
  scadenza dello slot (orario + intervallo) è passata e nessuna ronda del giro è stata avviata tra
  metà intervallo prima e la scadenza, apre l'incidente **`ronda_mancata`** («Ronda mancata
  (orario pianificato non svolto)») attribuito alla sede del giro. Escalation normale (gruppi,
  reperibilità, rete di ultima istanza); **non** è salva-vita: la manutenzione lo sospende.
  - Un solo incidente per slot (`app.patrol_missed`, UNIQUE(schedule_id, slot_at)).
  - Solo scadenze delle ultime 6 ore e mai slot precedenti alla creazione del piano: creare o
    riattivare un piano non genera allarmi arretrati.
  - Cruscotto vigilanza: ogni slot mostra l'ora locale; lo slot mancato ha ⚠ e apre l'allarme.
- `qa/patrol-missed-e2e.sh` **13/13**.

## Ronde e presenze dimenticate (CA-17, mig `0141`)

- **Una ronda in corso per giro**: avviare una seconda ronda sullo stesso giro mentre una è in
  corso dà 409 con chi l'ha avviata e a che ora (anche indice unico parziale, creato se i dati lo
  permettono).
- **Ronda abbandonata**: una ronda in corso oltre *finestra + tolleranza + 60 min* (8 ore se il giro
  non ha finestra) viene chiusa dal worker come **abbandonata** (`closed_by='auto'`) con anomalia
  «Ronda abbandonata (mai chiusa)»; il giro torna avviabile. Ronde sul wall-clock SQL, come avvio
  e punzonature.
- **Presenza di cantiere dimenticata**: aperta da oltre 16 ore → chiusa a **durata zero** e
  marcata *da verificare* (`auto_closed`): niente ore o costi inventati; la scheda commessa la
  segnala. Ingresso e uscita in cantiere ora usano l'orologio unico (`st.clock`).
- e2e `qa/stale-open-e2e.sh` **11/11**.

## Perimetro

Ronde = **Lavoro sul campo**. Dal 24/11/2026 (decisione PO, CA-12) il modulo produce un solo tipo
di allarme, la **ronda mancata**, e per questo porta con sé **Centrale ed escalation** anche senza
Sicurezza Lavoratore (`ESCALATION_MODS` e capability `incidents.*`/`escalation.manage` includono
`giri`). Niente SOS, check-in o dispositivi di sicurezza. Riusa `sites` (facoltativo) e i geofence
come riferimento d'area; indipendente dagli altri moduli campo.

**Assegnazione a gruppo (Tappa 3):** un giro può essere assegnato a un **gruppo di persone**
condiviso (`assigned_group`). In tal caso solo i membri (o il pianificatore) lo vedono e possono
avviare la ronda; i giri senza gruppo restano aperti a tutti. Dettaglio: `campo-gruppi.md`.

Prossimo possibile: finestra per fascia oraria/turno.
