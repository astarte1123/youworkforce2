# YouWorkforce — Manuale tecnico di configurazione e rilascio

Per chi **installa la piattaforma, configura le aziende clienti (tenant) e le consegna**, e per chi
prende in carico il progetto. Questo manuale dice **cosa fare e in che ordine**. Le schermate sono
descritte nel [manuale utente](manuale-utente.md), i dettagli di ogni area nei documenti elencati in
[§9](#cap9).

Aggiornato al 26/11/2026.

## Come usarlo

| Devi… | Vai a |
|---|---|
| capire il prodotto in poco tempo | [1. Il prodotto in breve](#cap1) |
| installare o aggiornare un ambiente | [2. Ambienti](#cap2) |
| **rilasciare un'azienda cliente** | [3. Procedura di rilascio](#cap3), poi [4](#cap4)–[7](#cap7) |
| configurare un'area (sicurezza, campo, integrazioni, IoT, AI) | [4. Configurazione per area](#cap4) |
| risolvere un problema | [8. Gestione e problemi](#cap8) |
| prendere in carico lo sviluppo | [10. Passaggio di consegne per sviluppatori](#cap10) |

---

<a id="cap1"></a>
## 1. Il prodotto in breve

YouWorkforce (YW) ha due grandi aree:

- **Sicurezza del lavoratore:** allarmi da badge GPS, app e sensori, centrale allarmi, escalation
  verso i reperibili finché qualcuno prende in carico;
- **Lavoro sul campo:** interventi, pianificazione, squadre, mezzi, ronde, turni, ore, consuntivi.

Ogni azienda cliente è un **tenant**, con dati separati. Ogni tenant ha i **moduli** che ha
comprato.

### Componenti

| Componente | Tecnologia | Porta | Ruolo |
|---|---|---|---|
| Core | Rust (Axum, SQLx) | 8080 (`YW_BIND`) | API, motori nativi (escalation, dispositivi, IoT, meteo, pianificazione), migrazioni all'avvio |
| Console | Go | 8081 | serve console, superadmin e app dell'operaio; inoltra `/api` al core |
| Database | PostgreSQL 18 | 5432 | dati di tutti i tenant, isolati con Row Level Security |
| Badge Teltonika | TCP nativo | 5027 (`YW_TELTONIKA_PORT`) | ricezione diretta dai badge GPS |
| Sensori IoT | MQTT | 1883 / 8883 TLS | telemetria |

### Indirizzi

| Pagina | Indirizzo |
|---|---|
| Console dell'azienda | `https://<slug>.<dominio>/app.html` |
| App dell'operaio (PWA) | `https://<slug>.<dominio>/worker/` |
| Superadmin | `https://<host di controllo>/superadmin.html` |

Il **dominio** dice al sistema di quale azienda si tratta: lo slug scelto alla creazione diventa
l'indirizzo.

### Moduli

I servizi di nucleo sono sempre attivi: utenti, gruppi, sedi, permessi, privacy, dashboard,
inventario dispositivi.

| Area | Codice | Nome | Richiede |
|---|---|---|---|
| Sicurezza | `loneworker` | Lone Worker e centrale allarmi | — |
| | `gps` | Localizzazione GPS e badge | — |
| | `prevenzione` | Prevenzione e checklist | — |
| Campo | `interventi` | Interventi | — |
| | `cantieri` | Cantieri | — |
| | `giri` | Ronde e vigilanza | — |
| | `workforce` | Forza lavoro e turni | — |
| | `consuntivi` | Ore, spese e consuntivi | — |
| | `qualifiche` | Qualifiche e scadenze | — |
| | `mezzi` | Mezzi e attrezzature | `interventi` |
| | `meteo` | Meteo operativo | `interventi` |
| | `analisi` | Analisi e stime | `interventi` |
| | `pianificazione` | Pianificazione automatica | `interventi` |
| IoT | `iot` | Dispositivi e IoT industriale | — |
| Trasversale | `assistant` | Assistente YouWorker.ai e diario vocale | — |

Un modulo che ne richiede un altro non si attiva senza: il superadmin lo segnala. I **pacchetti**
(Sicurezza lavoratore, Lavoro sul campo, IoT, Completo e quelli personalizzati) attivano più moduli
in un colpo solo.

### Ruoli dentro un'azienda

`tenant_admin` (amministratore), `responsabile`, `pianificatore`, `operatore` (centrale),
`reperibile`, `lavoratore`, `auditor`.

Ogni ruolo è un insieme di **permessi** (capability). Si possono creare **ruoli personalizzati** e
dare permessi ai gruppi: vedi [capability-layer.md](capability-layer.md) e
[custom-roles.md](custom-roles.md).

### Connessioni esterne

YW è autonomo. Le uniche connessioni verso l'esterno, tutte facoltative e configurate da noi:

| Connessione | Per cosa | Dove si configura |
|---|---|---|
| Twilio, Slack, relay email | consegna di SMS, chiamate, messaggi degli allarmi | Superadmin → Sistema → Canali di consegna |
| Fornitore AI (youworker.ai o altri) | assistente, agente, dettatura | Superadmin → tenant → Connessione AI |
| MET Norway | previsioni meteo, se il tenant sceglie questa fonte | Console del tenant → Meteo → Fonte delle previsioni |
| Autentika (YouCo) | login con l'account YouCo, in prova accanto alla password | Superadmin → tenant → Login con Autentika |
| OpenStreetMap | sfondo delle mappe nel browser degli utenti | — |

### Regole che non si discutono

- **Gli allarmi salva-vita non si perdono e non escono dal loro tenant.** Le sospensioni, la
  manutenzione e le licenze non bloccano mai SOS e uomo a terra.
- **Nessun dato di un'azienda è visibile a un'altra**, nemmeno per errore di configurazione: il
  database rifiuta i riferimenti tra aziende diverse.
- **Mai dati dimostrativi o orologio di prova in produzione:** `YW_SEED_DEMO`, `TEST_CLOCK`.

---

<a id="cap2"></a>
## 2. Ambienti

### Avvio

- **Locale (prove, demo):** `docker compose up -d --build`. Console su `:8081`, core su `:8080`.
  Crea anche le aziende demo `acme` e `globex`. Lo sviluppo senza Docker è descritto nel
  [README](../README.md).
- **Staging e produzione:** stessi componenti dietro HTTPS. Il core esegue da solo le migrazioni
  all'avvio: un aggiornamento è «nuova versione → riavvio».

### Checklist di un nuovo ambiente di produzione

Da fare una volta per server, prima di creare aziende.

- [ ] **HTTPS** davanti a console e core; `YW_COOKIE_SECURE=1`; `YW_TRUSTED_PROXIES` con gli
  indirizzi del proxy.
- [ ] **Dominio:** `YW_BASE_DOMAIN` (le aziende saranno `slug.<dominio>`, serve un DNS jolly
  `*.<dominio>`) e `YW_CONTROL_HOST` (host del superadmin).
- [ ] **Database:** `YW_DB_OWNER` (proprietario, per le migrazioni) e `YW_DB_RT` (ruolo
  applicativo `app_rt`). La prima migrazione crea `app_rt` con una password di sviluppo: in
  produzione **cambiala subito** (`ALTER ROLE app_rt PASSWORD '…'`) e aggiorna `YW_DB_RT`.
- [ ] **Primo superadmin:** `YW_SUPERADMIN_EMAIL` e `YW_SUPERADMIN_PASS` al primo avvio, poi toglile.
  Al primo accesso si attiva la **verifica in due passaggi**, obbligatoria.
- [ ] **Cifratura dei segreti** (Superadmin → Sistema → Cifratura dei segreti): imposta la
  **passphrase di recupero** (almeno 12 caratteri) e conservala fuori dal server. Senza, dopo un
  ripristino su un altro server le chiavi AI e i canali di consegna vanno reinseriti.
- [ ] **Canali di consegna** (Sistema → Canali): Twilio con numero mittente in formato
  internazionale, eventualmente Slack ed email. Finché mancano, la consegna è **simulata** e la
  Centrale lo segnala in rosso.
- [ ] **Monitoraggio** (Sistema → Configurazione monitoring): soglie e destinatari email/SMS degli
  alert di sistema; `YW_METRICS_TOKEN` per l'endpoint delle metriche.
- [ ] **Porte:**
  - 5027 (badge) solo da reti fidate (VPN, allowlist);
  - MQTT su TLS (`YW_MQTT_TLS_CERT`, `YW_MQTT_TLS_KEY`) e `YW_MQTT_PLAIN=off`.
- [ ] **Meteo con MET Norway:** `YW_METNO_CONTACT` = e-mail o sito dell'operatore (lo chiedono i
  termini del servizio).
- [ ] **Conservazione dei dati:** `YW_RETENTION_DAYS` secondo il contratto (0 = senza limite).
- [ ] **Assenti:** `YW_SEED_DEMO` e `TEST_CLOCK` non devono esserci.
- [ ] **Backup** (YW non lo fa da solo):
  - `pg_dump` giornaliero del database, conservato fuori dal server, con almeno un ripristino di
    prova;
  - la passphrase di recupero, insieme al backup ma in un posto diverso.

L'elenco completo delle variabili è nell'[appendice tecnica del manuale utente](manuale-utente.md#appendice).

---

<a id="cap3"></a>
## 3. Procedura di rilascio di un'azienda cliente

Otto fasi. Ogni fase ha una checklist: si passa alla successiva quando la precedente è spuntata.

```
F0 Raccolta → F1 Creazione → F2 Accessi → F3 Base → F4 Aree → F5 App operaio → F6 Collaudo → F7 Consegna
```

### F0 — Raccolta delle informazioni dal cliente

Prima di toccare il sistema, chiedere al cliente:

| Cosa | Perché | Esempio |
|---|---|---|
| Ragione sociale, nome breve per l'indirizzo | nome e slug | Idealverde → `idealverde` |
| Moduli / pacchetto acquistato | cosa attivare | Lavoro sul campo + Mezzi + Meteo + Pianificazione |
| Numero di utenti, app e dispositivi IoT | licenze | 66 giardinieri, 0 badge |
| Amministratore del cliente (nome, email) | invito | admin@cliente.it |
| Come entrano le persone | password o Autentika | Google aziendale → Autentika |
| Sedi operative e depositi, con indirizzo | sedi, punto di partenza delle squadre | «Sede operativa e deposito» |
| Elenco persone con ruolo, telefono e squadra | utenti, gruppi, escalation | file Excel/CSV |
| Clienti, sedi dei clienti, contratti con SLA | anagrafiche, SLA degli extra | estratto del gestionale |
| Tipi di lavoro, con durata, persone, patentini e mezzi | tipi di attività | taglio erba: 40 min + 0,05 min/m² |
| Mezzi e attrezzature, con scadenze | anagrafica mezzi | furgoni, piattaforme, revisioni |
| Patentini e qualifiche delle persone, con scadenza | qualifiche | PLE, motosega, fitosanitari |
| Gestionale (ERP) e software turni | integrazioni | chiave e CSV |
| Per la sicurezza: dispositivi (IMEI), reperibili, orari di copertura | dispositivi ed escalation | badge Teltonika GH-5200 |
| Logo e colori | identità visiva | PNG fino a 512 KB |

### F1 — Creazione dell'azienda (superadmin)

Superadmin → **Tenant** → **+ Nuovo tenant**.

- [ ] Nome e **slug**: lettere minuscole, cifre e trattino. Non cambia più: è l'indirizzo.
- [ ] **Pacchetto** o moduli. Quelli richiesti dagli altri si attivano da soli.
- [ ] **Licenze** app e IoT (vuoto = illimitato). Sono indicative: oltre il limite si vede l'uso in
  rosso, nulla si blocca.
- [ ] **Amministratore:** **invito monouso**, consigliato (link valido 7 giorni, da consegnare al
  cliente). La password diretta va bene solo per prove.
- [ ] Nel dettaglio dell'azienda:
  - **Identità visiva** (logo, colori);
  - **Visibilità mappe**;
  - **Strumenti per modulo** se servono.
- [ ] Se l'azienda usa l'AI: **Connessione AI** e **Verifica**.

### F2 — Accessi

- **Con password:** l'amministratore attiva l'account dall'invito, poi crea gli altri utenti (con
  invito o password iniziale).
- **Con Autentika** (in prova, i due accessi convivono). Procedura completa in
  [autentika-integrazione.md](autentika-integrazione.md):
  1. in Autentika, nel tenant del cliente, un client *web* con indirizzo di ritorno
     `https://<slug>.<dominio>/api/auth/oidc/callback` (identico, https) e scope
     `openid profile email groups`;
  2. Superadmin → tenant → **Login con Autentika**: issuer, client, segreto, indirizzo di ritorno,
     ruolo predefinito, corrispondenza dei ruoli;
  3. **Verifica collegamento**: tutti i controlli devono essere verdi. Poi **Attivo**;
  4. al primo accesso l'utente si collega a quello esistente con la stessa email verificata,
     oppure viene creato.

- [ ] L'amministratore del cliente è entrato.
- [ ] Se c'è Autentika, la verifica è verde e un utente di prova è entrato con il bottone.

### F3 — Configurazione di base (amministratore dell'azienda)

Nella console dell'azienda la voce **Configurazione** mostra la **configurazione guidata**: la lista
di cosa manca, calcolata dai moduli attivi ([configurazione-guidata.md](configurazione-guidata.md)).

- [ ] **Fuso orario** dell'azienda (pagina Sedi → «fuso orario»).
- [ ] **Sedi**: indirizzo **con posizione** (serve per mappe, spostamenti e meteo) e, dove serve,
  orari in cui si può lavorare e note di accesso.
- [ ] **Utenti** con ruolo e telefono (Persone e permessi). Gli utenti si inseriscono uno per uno o
  con l'invito; da CSV si importano solo clienti, sedi, commesse e interventi.
- [ ] **Gruppi**: le **squadre** sono gruppi di tipo «squadra» (servono a pianificazione e app
  dell'operaio); poi gruppi di allarme e reparti.
- [ ] **Permessi:** ruoli personalizzati solo se i ruoli standard non bastano.
- [ ] **Sedi consentite** per chi deve vedere solo alcune sedi.

### F4 — Configurazione per area

Vedi [§4](#cap4): fare solo le parti dei moduli attivi.

### F5 — App dell'operaio

Vedi [§5](#cap5).

### F6 — Collaudo prima della consegna

Vedi [§6](#cap6). **Non si consegna senza collaudo.**

### F7 — Consegna

Vedi [§7](#cap7).

---

<a id="cap4"></a>
## 4. Configurazione per area

<a id="cap4-sicurezza"></a>
### 4.1 Sicurezza del lavoratore (`loneworker`, `gps`)

Ordine consigliato. Dettagli in [escalation-manuale.md](escalation-manuale.md) e
[dispositivi-gps.md](dispositivi-gps.md).

1. **Recapiti:** il telefono di ogni reperibile va **verificato** (codice via SMS). Chi non ha il
   telefono verificato non riceve.
2. **Gruppi di allarme** e **gruppo on-call predefinito**: senza, un allarme critico non va a
   nessuno.
3. **Instradamento** (routing) degli allarmi per sede o tipo, e **rete di ultima istanza**.
4. **Profili di protezione e trigger:** silenzio del dispositivo, immobilità, check-in, aree. Se
   devono valere «solo durante il turno» serve `workforce`.
5. **Dispositivi:**
   - badge Teltonika: IMEI registrato in YW; sul badge server `<host>` e porta `5027`;
     poi **Collegamento → verifica ora** e **Metti in servizio**;
   - app: basta l'utente;
   - un IMEI registrato in due aziende viene rifiutato: Superadmin → Sistema → Dispositivi
     rifiutati.
6. **Consegna reale:** i canali del server devono essere configurati (§2). La Centrale non deve
   mostrare l'avviso rosso «consegna simulata».

<a id="cap4-campo"></a>
### 4.2 Lavoro sul campo

Ordine consigliato, perché ogni passo usa i precedenti. Dettagli in
[lavoro-campo-fase1.md](lavoro-campo-fase1.md) e nei documenti indicati.

1. **Clienti** e loro **sedi** (con posizione, orari, accesso).
2. **Contratti** con **SLA in ore**: serve a misurare gli extra gestiti in tempo.
3. **Listino** (manodopera, materiali, trasferte), se ci sono consuntivi.
4. **Qualifiche** (`qualifiche`) e la loro assegnazione alle persone, **con scadenza**.
5. **Mezzi** (`mezzi`): tipo (es. «furgone», «piattaforma»), stato, qualifica richiesta per l'uso,
   **scadenze** (revisione, tagliando).
6. **Tipi di attività**:
   - unità di misura, durata base e minuti per unità, persone necessarie;
   - patentini e mezzi richiesti;
   - con `meteo`, i **limiti meteo** (pioggia, vento, temperature, ore asciutte dopo).
7. **Piani di manutenzione** (ricorrenze a contratto): generano da soli gli interventi.
8. **Meteo** (`meteo`), pagina Meteo ([meteo-operativo.md](meteo-operativo.md)):
   - **zone** con posizione (es. Milano Nord, Monza); ogni sede usa la zona più vicina o quella
     fissata;
   - **fonte delle previsioni**: **MET Norway** (YW scarica da solo, gratuito) oppure **sistema
     collegato** (Youco con una chiave);
   - controllare che ogni zona sia «aggiornata» e senza errori.
9. **Pianificazione automatica** (`pianificazione`), Pianificazione → Automatica → Impostazioni
   ([pianificazione-automatica.md](pianificazione-automatica.md)):
   - **deposito** (generale o per squadra), orario e giorni di lavoro, straordinario ammesso;
   - velocità media, fattore di percorso, minuti di sistemazione;
   - meteo da evitare o ignorare.

   Senza deposito non si stimano gli spostamenti.
10. **Pilota** (facoltativo, consigliato per i clienti nuovi): due squadre per quattro settimane,
    con le ore di pianificazione di oggi dichiarate dal cliente.
11. **Analisi** (`analisi`): non si configura. Servono interventi chiusi con il tempo reale; con
    lo storico importato si vede subito.

<a id="cap4-integrazioni"></a>
### 4.3 Integrazioni (gestionale, software turni, Youco)

Dettagli in [integrazioni.md](integrazioni.md).

- [ ] **Chiave di integrazione** per ogni sistema esterno (Configurazione → Integrazioni):
  - solo i permessi che servono, con scadenza;
  - la chiave si vede **una volta sola**: consegnala in modo sicuro;
  - una chiave revocata smette di funzionare subito.
- [ ] **Codici esterni:** clienti, sedi, commesse e interventi del gestionale entrano con il loro
  codice (`PUT /api/integration/…/{codice}`). Lo stesso codice aggiorna senza creare doppioni.
- [ ] **Import CSV** per il primo caricamento: clienti, sedi, commesse, interventi. Prima riga con le
  colonne, `ref` = codice del gestionale. Le righe con errori sono nel rapporto e non fermano le
  altre.
- [ ] **Ritorno verso il gestionale:** `GET /api/integration/changes?since=…` (interventi chiusi,
  consuntivi).

<a id="cap4-iot"></a>
### 4.4 IoT (`iot`)

- **Scenari:** Superadmin → tenant → Scenari IoT, oppure catalogo in Superadmin → Scenari.
- **Regole dei sensori:** soglie e allarmi. I sensori trasmettono via MQTT o HTTP con le loro
  credenziali.
- Dettagli in [iot-nativo.md](iot-nativo.md).

<a id="cap4-ai"></a>
### 4.5 Assistente AI (`assistant`)

1. Superadmin → **Connessione AI** e **Verifica**.
2. L'amministratore del cliente conferma l'avviso privacy e accende le funzioni: dettatura,
   agente.

Senza connessione le funzioni AI restano spente; il resto non cambia.

---

<a id="cap5"></a>
## 5. App dell'operaio

L'app si compone dai moduli attivi: chi ha solo sicurezza vede SOS e check-in, chi fa lavoro sul
campo vede la giornata e la mappa. Descrizione delle schermate nel
[manuale utente, capitolo 4](manuale-utente.md#cap4); progetto in [app-operaio.md](app-operaio.md).

**Distribuzione:** è una web app installabile (PWA), non passa dagli store.

1. Dare all'operaio l'indirizzo `https://<slug>.<dominio>/worker/` (anche con un QR code stampato).
2. Sul telefono:
   - Android/Chrome: menu → **Aggiungi a schermata Home** o **Installa app**;
   - iPhone/Safari: **Condividi → Aggiungi a Home**.
3. Primo accesso con le credenziali dell'operaio, o con Autentika se attivo.
4. Concedere il permesso di **posizione**: serve a mappa, condivisione della posizione e SOS.

**Cosa deve essere pronto perché la giornata sia completa:**

- interventi assegnati all'operaio o alla sua **squadra**, con data e ora;
- sedi **con posizione**;
- **deposito** impostato nella pianificazione, per gli spostamenti;
- **zone meteo** aggiornate, per il meteo.

Dopo un aggiornamento l'app si rinnova da sola. Se un telefono mostra la versione vecchia, basta
chiudere e riaprire l'app.

---

<a id="cap6"></a>
## 6. Collaudo prima della consegna

Si fa con l'amministratore del cliente presente, o almeno con un utente reale per ruolo. Spuntare
solo ciò che riguarda i moduli attivi.

**Generale**
- [ ] La configurazione guidata è completa (100%) o i passi mancanti sono motivati.
- [ ] Ogni ruolo entra e vede il suo menu. Il lavoratore non vede prezzi né pagine di gestione.
- [ ] Logo, colori e lingua sono giusti.

**Sicurezza del lavoratore** (obbligatorio se attiva)
- [ ] **SOS reale** dall'app e da un badge: l'allarme arriva in Centrale **e** al reperibile (SMS
  e chiamata), la presa in carico ferma l'escalation.
- [ ] Silenzio del dispositivo e immobilità scattano secondo i profili, e si chiudono.
- [ ] La Centrale non mostra «consegna simulata».

**Lavoro sul campo**
- [ ] Un intervento creato a mano rispetta orari della sede, patentini e mezzi: un conflitto vero
  viene segnalato.
- [ ] Con `pianificazione`: una proposta per la settimana ha **0 vincoli non rispettati**; gli
  interventi non pianificati hanno un motivo comprensibile.
- [ ] Con `meteo`: le zone sono aggiornate; un tipo di attività con limite mostra l'avviso nei
  giorni di pioggia.
- [ ] **App dell'operaio** con un operaio vero: vede la sua giornata in ordine, la mappa, il
  meteo; segna arrivo, avvio e chiusura con foto; il pianificatore vede l'esito.
- [ ] Consuntivo da un intervento chiuso: righe e tariffe giuste.

**Integrazioni**
- [ ] Il gestionale invia clienti, sedi e interventi con la chiave: nessun doppione al secondo
  invio.
- [ ] Il ritorno delle modifiche verso il gestionale funziona.

---

<a id="cap7"></a>
## 7. Consegna al cliente

**Cosa consegnare:**

| Cosa | A chi |
|---|---|
| Indirizzo della console `https://<slug>.<dominio>/app.html` e **invito dell'amministratore** (se non usato) | amministratore |
| Indirizzo dell'app `…/worker/`, anche con QR code, e le istruzioni di installazione del §5 | operai, tramite l'amministratore |
| Chiavi di integrazione (una volta sola, canale sicuro) | referente IT del gestionale |
| **Manuale utente**, indicando i capitoli per ruolo (Guida rapida per ruolo, cap. 1) | tutti |
| Verbale di collaudo (la checklist del §6 compilata) | cliente e archivio interno |
| Contatti dell'assistenza | amministratore |

**Formazione consigliata**, per ruolo e breve:

- amministratore: utenti, gruppi, sedi, permessi;
- pianificatore: interventi, agenda, pianificazione automatica, meteo;
- operatore di centrale: allarmi, presa in carico;
- operai: app, 15 minuti con il telefono in mano.

---

<a id="cap8"></a>
## 8. Dopo il rilascio: gestione e problemi

### Gestione ordinaria (superadmin)

- **Assistenza:** **Impersona admin** per vedere ciò che vede l'amministratore. Uscendo si torna
  alla propria identità. Resta traccia nel registro attività.
- **Licenze e moduli:** si cambiano dalla scheda dell'azienda, con effetto immediato.
- **Sospensione:** subito o alla data, con motivo. Un'azienda sospesa è ferma del tutto: allarmi,
  app e dispositivi compresi. **Riattiva** fa ripartire l'escalation degli allarmi rimasti aperti.
- **Eliminazione:** definitiva; per confermare si riscrive lo slug. Prima fare un'esportazione o un
  backup se il contratto lo richiede.
- **Stato del sistema:** semaforo e storico alert in Sistema. Le soglie si impostano lì.

### Aggiornamento di versione

1. Backup del database.
2. Nuova versione del core e della console.
3. Riavvio: le migrazioni partono da sole e sono solo in avanti.
4. Controllo di Sistema → Stato sistema e di una pagina per area.

Le migrazioni non si modificano mai dopo il rilascio: si aggiunge sempre una migrazione nuova.

### Problemi tecnici comuni

| Sintomo | Causa probabile | Cosa fare |
|---|---|---|
| Centrale in rosso «consegna simulata» | canali di consegna assenti | Superadmin → Sistema → Canali |
| Chiavi AI o canali «da reinserire» dopo un ripristino | chiave dei segreti bloccata sul server nuovo | Sistema → Cifratura → sblocca con la passphrase; se persa, **Azzera** e reinserisci |
| Un badge non si collega | IMEI sbagliato, server o porta sbagliati, IMEI in due aziende, modulo spento | scheda del badge → Collegamento → verifica; Sistema → Dispositivi rifiutati |
| Meteo vuoto | nessuna zona, fonte non scelta, o errore di MET Norway | Meteo → zone e fonte; l'errore è scritto sulla zona |
| MET Norway «ha rifiutato la richiesta» | manca il contatto dell'operatore | impostare `YW_METNO_CONTACT` e riavviare |
| La pianificazione non stima gli spostamenti | manca il deposito o le sedi non hanno posizione | Impostazioni della pianificazione; sedi |
| Interventi «non pianificati: nessuna squadra con la qualifica» | qualifica mancante o scaduta in tutte le squadre | aggiornare le qualifiche delle persone |
| «Login con Autentika non attivo» o «client non valido» | configurazione o segreto sbagliati | Superadmin → Login con Autentika → Verifica collegamento |
| L'app dell'operaio non mostra la giornata | interventi senza data o non assegnati a lui o alla sua squadra | assegnare con data e ora; verificare che sia nella squadra |
| Un utente è bloccato per qualche minuto | troppi tentativi di accesso sbagliati | attendere 5 minuti |

I problemi lato utente sono nel [manuale utente, capitolo 10](manuale-utente.md#cap10).

---

<a id="cap9"></a>
## 9. Documenti di dettaglio

L'indice completo per pubblico è in [README.md](README.md).

| Area | Documento |
|---|---|
| Schermate e uso quotidiano | [manuale-utente.md](manuale-utente.md) |
| Registro delle funzioni e delle decisioni | [manuale.md](manuale.md) |
| Configurazione guidata | [configurazione-guidata.md](configurazione-guidata.md) |
| Escalation e reperibilità | [escalation-manuale.md](escalation-manuale.md) |
| Dispositivi GPS e badge | [dispositivi-gps.md](dispositivi-gps.md) |
| Ronde | [ronde-manuale.md](ronde-manuale.md) |
| Lavoro sul campo: tipi di attività, mezzi, vincoli | [lavoro-campo-fase1.md](lavoro-campo-fase1.md) |
| Meteo operativo | [meteo-operativo.md](meteo-operativo.md) |
| Analisi e stime | [analisi-stime.md](analisi-stime.md) |
| Pianificazione automatica e pilota | [pianificazione-automatica.md](pianificazione-automatica.md) |
| App dell'operaio | [app-operaio.md](app-operaio.md) |
| Integrazioni | [integrazioni.md](integrazioni.md) |
| Autentika | [autentika-integrazione.md](autentika-integrazione.md) |
| IoT | [iot-nativo.md](iot-nativo.md) |
| Permessi e ruoli | [capability-layer.md](capability-layer.md) · [custom-roles.md](custom-roles.md) |
| Privacy e audit | [gdpr-privacy-audit.md](gdpr-privacy-audit.md) |
| Isolamento tra aziende | [sicurezza-isolamento-tenant.md](sicurezza-isolamento-tenant.md) |
| Glossario | [glossario.md](glossario.md) |

---

<a id="cap10"></a>
## 10. Passaggio di consegne per sviluppatori

### Struttura del repository

| Cartella | Contenuto |
|---|---|
| `core/` | il server Rust: `src/` per area (es. `planning.rs`, `weather.rs`, `autentika.rs`, `myday.rs`), `migrations/` numerate |
| `console/` | server Go e pagine statiche: `static/app.html` (console), `static/superadmin.html`, `static/worker/` (app dell'operaio), `static/i18n*.js` (traduzioni) |
| `qa/` | test end-to-end in bash (`*-e2e.sh`), `run-all.sh`, controlli statici, finti servizi esterni (`metno_mock.py`, `oidc_mock.py`) |
| `docs/` | documentazione (indice in [README.md](README.md)) |
| `.github/workflows/ci.yml` | CI: build, unit test, suite e2e del core |

### Build e test

- **Build:** `cd core && cargo build --release`; `cd console && go build`.
- **Unit test:** `cd core && cargo test`.
- **Test end-to-end:** con il core avviato in modalità di prova (`TEST_CLOCK=1 YW_SEED_DEMO=1`),
  dalla radice del repository `BASE=http://<core>:8080 MODE=core bash qa/run-all.sh`. Sono circa
  150 suite. `silence-shift-e2e.sh` è instabile solo dentro la regressione completa: da sola
  passa.
- **Controlli statici**, da far passare prima di ogni commit:

| Script | Cosa controlla |
|---|---|
| `qa/handler_guards.py` | ogni rotta controlla modulo e permesso |
| `qa/owner_pool_audit.py` | ogni query privilegiata filtra per azienda |
| `qa/console-syntax.sh` | sintassi delle pagine |
| `qa/glossary-check.py` | nomi vecchi vietati, es. «giro» al posto di «ronda» |

### Regole del progetto

- **Documentazione nello stesso commit del codice.**
- **Migrazioni:** mai modificate dopo l'applicazione; una nuova migrazione per ogni cambiamento.
  Quelle che aggiungono riferimenti rigenerano il controllo «stesso tenant» (vedi le ultime).
- **Isolamento:**
  - le query sul pool privilegiato hanno sempre `tenant_id`, oppure una voce motivata in
    `qa/owner_pool_allow.txt`;
  - le query normali passano da `db::begin_tenant` (Row Level Security).
- **Orologio:** il tempo di dominio è `st.clock.now()` (controllabile nei test); l'orologio di
  sistema solo per cache e servizi esterni.
- **Interfaccia comprensibile:** mai codici grezzi; nomi chiari e una spiegazione. I testi nuovi
  vanno tradotti in inglese (`console/tools/i18n/`: `extract.py`, poi `build.py --missing` e
  `build.py`).
- **Moduli:** ogni funzione nuova appartiene a un modulo, registrato in `modules_catalog` (con una
  migrazione) e nel `Manifest` di `core/src/modules.rs`. Le dipendenze sono dichiarate lì.
- **Connessioni esterne:** solo quelle del §1; ogni nuova va decisa dal PO.

### Stato e cose aperte (26/11/2026)

- **Fatto per il lavoro sul campo**, a partire dal caso Idealverde:
  - fasi 1–6: vincoli, integrazioni, analisi, meteo, pianificazione automatica, pilota;
  - app dell'operaio M1 con la nuova interfaccia;
  - login con Autentika in prova.
- **Da fare:**
  - app dell'operaio M2–M4;
  - fasi A2–A5 di Autentika: allineamento utenti, licenze, migrazione, chiamate all'AI;
  - import dei dati reali di Idealverde;
  - pianificazione per singola persona e con i turni;
  - rilettura madrelingua dell'inglese;
  - stabilizzazione di `silence-shift`;
  - test reale del badge GH-5200;
  - build di rilascio.
- Piano e decisioni del caso Idealverde:
  [analisi-miglioramento-yw.md](analisi-miglioramento-yw.md) e i documenti di fase in §9.
