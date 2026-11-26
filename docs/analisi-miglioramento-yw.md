# YouWorkforce: analisi del codice e progetto di miglioramento (codice, UI, UX)

Data: 23/11/2026. Branch analizzato: `feat/worker-safety-perimeter` (commit `e377bc5`).

Il documento ha due parti. La prima misura il problema e ne individua le cause. La seconda è un
progetto di miglioramento con fasi, regole e criteri di accettazione verificabili.

---

## Parte 1. Analisi

### 1.1 Sintesi

L'impressione è corretta e i numeri la confermano. In 15 giorni (08/11 → 23/11) il codice è
cresciuto di circa 4 volte, quasi tutto in **due soli file**. La crescita non è venuta da un
prodotto più capace nel suo nucleo (proteggere un lavoratore), ma dall'accumulo di funzioni
importate per "parità" con tre prodotti esterni (GoAlert, Traccar, ThingsBoard) che poi sono stati
ritirati, lasciando nel codice e nell'interfaccia il loro vocabolario e le loro schermate.

L'usabilità è peggiorata per la stessa ragione: ogni funzione nuova ha trovato posto come nuova voce
di menu o nuova sezione in fondo a una pagina esistente, senza mai ridisegnare il percorso
dell'utente.

### 1.2 I numeri

| Indicatore | Valore | Commento |
|---|---|---|
| `core/src/api.rs` | **911 KB, 12.748 righe** | un solo file con 512 handler, 815 query SQL inline |
| crescita `api.rs` | 209 KB (09/11) → 911 KB (23/11) | ×4,4 in due settimane |
| `console/static/app.html` | **578 KB, 5.891 righe, 618 funzioni** | tutta la console in un file |
| crescita `app.html` | 93 KB (09/11) → 578 KB (23/11) | ×6,2 in due settimane |
| route HTTP | 335 | 12 ancora sotto `/api/traccar/*` |
| voci di menu statiche | **35**, in 5 gruppi | più quelle dinamiche degli scenari IoT |
| migrazioni | 124 in 15 giorni | `0024` da sola è un dump catalogo di 2.202 righe |
| uso di `owner_pool` (bypassa RLS) | **387** contro 247 di `rt_pool` | l'isolamento tenant dipende dalla disciplina |
| `.unwrap()` in `api.rs` | 363 | ogni unwrap in un handler è un potenziale panic |
| test unitari in `api.rs` | **0** | la rete di sicurezza è fatta solo da 111 script e2e |
| stili inline in `app.html` | **1.153** `style="..."`, 411 valori `px` | la regola "design token" non è applicata |
| handler inline `onclick=` | 455 | logica sparsa dentro stringhe HTML |
| `innerHTML` | 234 | rendering a stringhe, rischio XSS gestito a mano (810 `esc(`) |
| testi tradotti (`t('...')`) | 91 | i18n ferma al guscio, le viste sono solo in italiano |
| variabili d'ambiente `YW_*` | 34 | configurazione di prodotto sparsa tra env e DB |
| documenti | 25 in `docs/`, 16 in `contracts/`, 1 prompt in root | tre manuali "parity" di prodotti ritirati |
| commit | 83 `feat`, 31 `fix` | molti fix riparano regressioni del ciclo precedente |

### 1.3 Problemi del codice sorgente

**C1. Monolite backend.** `api.rs` contiene autenticazione, sicurezza lavoratore, escalation,
ingest dispositivi, cantieri, interventi, consuntivi, IoT, AI, superadmin e privacy. Effetti già
osservati in questo progetto:
- compilazione lenta e due casi di stack overflow di `rustc` (risolti con `debug=1` e con lo split
  del router: sintomi, non cure);
- nessun confine tra domini, quindi ogni modifica può rompere un'area lontana (vedi la serie di fix
  "autonomia moduli");
- revisione del codice di fatto impossibile su un file da 900 KB.

**C2. Boilerplate ripetuto in ogni handler.** Ogni handler ripete a mano
`require_tenant` → `s.tenant_id.unwrap()` → `require_module` → `require_cap` → `begin_tenant`. Una
dimenticanza non dà errori di compilazione: dà una falla. È esattamente l'origine del fix
`0f093ad` (allarmi senza `tenant_id`).

**C3. Isolamento tenant affidato alla memoria.** 387 query passano da `owner_pool`, che ignora la
RLS. La regola "mai query incidenti senza tenant_id" esiste solo come nota, non come vincolo del
codice. Per un prodotto salva-vita multi-tenant è il rischio tecnico numero uno.

**C4. Codice di connettori ritirati ancora vivo.** La migrazione `0078` ha ritirato
GoAlert/Traccar/ThingsBoard, ma restano:
- 12 route `/api/traccar/*`; alcune (`drivers`, `maintenance`, `computed`, `accumulators`, `reports`)
  fanno ancora chiamate HTTP a un server Traccar esterno (`tc_conn_or_err`, `st.http.get(...)`);
- 320 occorrenze di "traccar" in `api.rs` e 69 nella console;
- il device Teltonika nativo è salvato con `source='traccar'`: un nome che non descrive più nulla;
- in UI compaiono "Traccar avanzato", "Traccar — salute connessione", "Registra anche su Traccar",
  "Inoltro eventi da un server Traccar esistente", e la voce di menu **Connettori**.

**C5. Dati dentro le migrazioni.** Cataloghi (dispositivi, sensori IoT) caricati tramite migrazioni
da migliaia di righe. Le migrazioni devono cambiare lo schema; i dati di catalogo vanno in seed
versionati, aggiornabili senza una nuova migrazione.

**C6. Frontend monolitico a stringhe.** Una sola pagina da 578 KB in cui ogni vista costruisce HTML
concatenando stringhe, con stile inline e `onclick` inline. Conseguenze: nessun componente
riusabile reale (51 `<table class="table">` costruite a mano, due definizioni di `emptyRow`),
aspetto incoerente tra le pagine, impossibile fare un responsive serio, i18n bloccata.

**C7. Codice morto.** Primi candidati trovati in modo automatico (da confermare prima di
rimuovere, perché alcuni potrebbero essere invocati da stringhe costruite a runtime): console
`addCircle`, `commesseCSV`, `devFilter`, `fmtDT`, `interventiCSV`, `listinoCSV`,
`saveIndoorGeofence`, `setMapMode`, `siteRen`, `statusColor`; backend `admin_only`,
`esc_synth_uuid`, `esc_user_rotation_shifts`. A questi si somma tutto il codice proxy verso
Traccar/ThingsBoard/GoAlert (C4) e gli "scenari" IoT dimostrativi (122 occorrenze).

**C8. Errori come panic.** 363 `unwrap()` e 123 `unwrap_or_default()`: nel primo caso un dato
inatteso abbatte la richiesta (o il worker); nel secondo un errore diventa silenziosamente un
valore vuoto. In un sistema di allarmi entrambi sono inaccettabili.

**C9. Test sbilanciati.** 111 script e2e in shell, zero test unitari sui domini. Gli e2e sono lenti,
dipendono dall'ambiente (esempio recente: falsi FAIL della policy di escalation senza il ricevitore
su `:18099`) e non proteggono la logica pura (calcolo escalation, mappatura allarmi, permessi).

**C10. Documentazione dispersa.** Tre manuali "parity" di prodotti non più integrati, piani in
`contracts/`, un prompt di orchestrazione in root. Chi entra nel progetto non sa quale documento
descrive il prodotto di oggi. Il manuale unico è ancora un TODO.

### 1.4 Problemi di UI e UX

**U1. Menu troppo lungo e ridondante.** 35 voci statiche. Duplicati e quasi-duplicati:

| Area | Voci oggi | Problema |
|---|---|---|
| Mappe | Mappa allarmi, Mappa dispositivi, Mappa cantieri, Piantine Indoor (×2, stesso nome) | quattro mappe per la stessa cosa: "dove sono persone e cose" |
| Pianificazione | Calendario, Turni, Agenda tecnici | tre calendari diversi |
| Persone | Utenti e ruoli, Utenti e sedi | la parola "Utenti" in due voci diverse |
| AI | Agente AI, AI · youworker.ai | uso e configurazione separati senza motivo evidente |
| Impostazioni | Configurazione, Moduli, Connettori | tre ingressi per configurare; "Connettori" punta a integrazioni ritirate |

**U2. La configurazione della sicurezza è spezzata in quattro pagine.** Per proteggere un lavoratore
l'amministratore deve conoscere la catena *dispositivo → trigger → profilo di protezione → routing →
gruppo di escalation*, distribuita tra **Dispositivi & App**, **Trigger di sicurezza**, **Profili di
protezione** ed **Escalation e reperibilità**. Il modello è giusto per il motore, sbagliato per la
persona: l'admin ragiona per lavoratore ("Mario è protetto?"), non per tabella.

**U3. Pagine "cassetto".** *Escalation e reperibilità* contiene 12 sezioni in un'unica pagina:
gruppo di default, gruppi, routing eventi, notifiche, manutenzione, il mio calendario reperibilità,
recapiti, assenze e sostituzioni, pianificazione temporanea, diagnostica motore, heartbeat monitor,
ingress email. Sono mescolati tre pubblici: chi configura, il reperibile che consulta il proprio
turno, il tecnico che fa diagnostica.

**U4. Linguaggio tecnico in interfaccia.** "catch-all", "heartbeat monitor", "ingress email",
"token di ingest", "Traccar", "codec", "backfill da cursore". Viola la regola del progetto
"mai codici grezzi in UI". L'utente tipo (responsabile sicurezza, capo cantiere) non conosce questi
termini.

**U5. Nessuna gerarchia di priorità.** Un allarme uomo-a-terra e una scadenza di contratto vivono
nello stesso stile visivo e nello stesso menu. Per un prodotto salva-vita l'allarme attivo deve
dominare ogni schermata, per ogni ruolo che deve reagire.

**U6. Incoerenza visiva.** 1.153 stili inline fanno sì che spaziature, tabelle, bottoni e stati vuoti
cambino da pagina a pagina. L'utente percepisce "tante app diverse" incollate insieme.

**U7. Mobile e lingua.** Il responsive della console e la traduzione delle viste sono ancora aperti.
Il reperibile, che riceve l'allarme sul telefono, oggi usa una console pensata per desktop.

**U8. Onboarding dispositivo ancora "da tecnico".** Lo scanner è un buon passo, ma il percorso
completo (scansione → assegnazione al lavoratore → prova allarme → conferma che la catena funziona)
non esiste come flusso unico. È il momento della verità per l'adozione del prodotto.

### 1.5 Cause di fondo (di processo, non di persone)

1. **Sviluppo per "parità".** Si è preso a modello l'insieme delle funzioni di GoAlert, Traccar e
   ThingsBoard e se ne sono chiusi i "gap" uno per uno. Il risultato è la somma di tre prodotti,
   non un prodotto. Quando i connettori sono stati ritirati, le funzioni sono rimaste.
2. **Aggiungere senza togliere.** Nessuna funzione è stata rimossa o fusa; ogni richiesta è
   diventata una voce di menu o una sezione nuova.
3. **Nessun budget strutturale.** Nessun limite a dimensione dei file, numero di voci di menu,
   stili inline. Le regole esistono nella memoria del progetto ma non nella CI.
4. **Verifica solo end-to-end.** Si verifica che "funziona", non che "si capisce". Manca la prova
   d'uso con l'utente reale su percorsi critici misurati.

---

## Parte 2. Progetto di miglioramento

### 2.1 Principi

1. **Il nucleo è proteggere il lavoratore.** Tutto il resto (campo, IoT, AI) si appoggia al nucleo
   e non lo complica.
2. **Ogni funzione ha una persona e un percorso.** Se non si sa chi la usa e in quale passo del suo
   lavoro, non entra in menu.
3. **Togliere vale quanto aggiungere.** Ogni fase ha un obiettivo di righe/voci rimosse.
4. **Le regole stanno nel codice e nella CI, non nella memoria.** Isolamento tenant, token di design,
   dimensione file: vincoli automatici.
5. **Nessuna regressione funzionale.** Ogni passo di refactoring è a comportamento invariato e passa
   la suite `qa/run-all.sh` prima del merge.

### 2.2 Fasi

Le stime sono indicative, per una persona a tempo pieno con assistenza AI. Le fasi 1 e 2 possono
procedere in parallelo; la 3 dipende dalla 2.

#### Fase 0. Fermare la crescita e fissare la base (3-4 giorni)

- **Freeze funzionale** sulle nuove feature fino alla fine della fase 2, eccetto fix su sicurezza
  lavoratore.
- **Misure di partenza** salvate in `qa/metrics-baseline.json` (le tabelle del §1.2) e script
  `qa/metrics.sh` che le ricalcola.
- **Guardrail in CI** (inizialmente in modalità "avviso", poi bloccanti):
  - nessun file sorgente oltre 1.500 righe (con lista di eccezioni che può solo diminuire);
  - `clippy::unwrap_used` vietato nei moduli handler;
  - conteggio di `style="` e `onclick=` che può solo scendere;
  - numero di voci `NAV` che può solo scendere.
- **Decisione su ciò che si ritira** (vedi §2.4), da confermare con il product owner.

#### Fase 1. Backend modulare e sicuro per costruzione (2-3 settimane)

1. **Estrattore unico di contesto.** Un extractor axum `TenantCtx` che risolve sessione, tenant,
   modulo e capability in un passo e restituisce errore tipizzato. Gli handler dichiarano cosa
   serve, non lo ripetono:
   ```rust
   async fn get_commessa(ctx: TenantCtx<Mod<"cantieri">, Cap<"cantieri.view">>, Path(id): Path<Uuid>) -> R
   ```
   (forma indicativa: in pratica tipi marker o una macro, da scegliere nello spike iniziale).
2. **Accesso al DB solo tramite `TenantDb`.** Un wrapper che apre la transazione con il tenant
   impostato (RLS attiva). `owner_pool` resta accessibile solo ai moduli di sistema (worker,
   superadmin, migrazioni) e un test di CI fallisce se compare altrove. Obiettivo: uso di
   `owner_pool` negli handler tenant = 0.
3. **Split di `api.rs` per dominio**, spostamento meccanico prima, pulizia dopo:
   ```
   core/src/
     http/        router, ctx (TenantCtx), errori, risposte
     safety/      incidenti, trigger, profili, check-in, SOS, centrale
     escalation/  motore (già escalation.rs), delivery, reperibilità, turni on-call
     devices/     catalogo, onboarding kit, ingest (teltonika, osmand, http), posizioni
     field/       interventi, clienti, listino, contratti, richieste, dispatch
     sites/       cantieri, commesse, SAL, costi, diario
     workforce/   turni, consuntivi, presenze
     patrol/      ronde, checkpoint, vigilanza
     iot/         sensori, regole, monitoraggio
     ai/          provider, agente, tool
     admin/       utenti, ruoli, gruppi, moduli, privacy, audit
     platform/    superadmin, licenze, branding, system snapshot
   ```
   Ogni modulo espone `routes()` e un `repo.rs` con le query. Nessun file oltre 1.500 righe.
4. **Errori tipizzati.** Un `AppError` (con `thiserror`) al posto di `unwrap()` e di
   `unwrap_or_default()` sui dati. Gli errori di dominio diventano messaggi chiari per l'utente.
5. **Rimozione del codice dei connettori ritirati** (dopo conferma §2.4): route `/api/traccar/*` che
   chiamano server esterni, poller, tabelle `traccar_*` non più usate, codice GoAlert/ThingsBoard
   residuo. Rinomina `source='traccar'` → `source='edge'` con migrazione e alias di compatibilità.
6. **Cataloghi fuori dalle migrazioni.** Catalogo dispositivi e sensori in `core/seed/*.json`,
   caricati all'avvio con upsert per versione. Per le nuove installazioni: una migrazione
   **baseline** che sostituisce le 124 attuali (le installazioni esistenti restano compatibili).
7. **Test unitari sul dominio puro.** Mappatura allarmi (IO → etype), stati dell'escalation,
   permessi, calcoli di consuntivo, attribuzione temporale. Obiettivo: le regole critiche hanno test
   veloci che girano a ogni build; gli e2e restano per i percorsi completi.

**Criteri di accettazione Fase 1:** nessun file `.rs` oltre 1.500 righe; zero `owner_pool` negli
handler tenant; zero `unwrap()` negli handler; route `/api/traccar/*` rimosse o rinominate; suite
e2e completa verde; tempo di compilazione incrementale ridotto (misurato prima e dopo).

#### Fase 2. Frontend a componenti, senza build step (2-3 settimane)

Si mantiene la scelta self-contained (nessun bundler, nessun framework pesante), ma si passa ai
**moduli ES nativi** serviti dalla console Go:

```
console/static/
  app.html          guscio: layout, menu, router (obiettivo < 30 KB)
  js/core/          api, router, i18n, auth, store
  js/ui/            componenti: table, form, field, drawer, modal, tabs, empty-state,
                    confirm, badge-stato, mappa, scanner
  js/views/         una vista per file (safety/centrale.js, devices/onboarding.js, ...)
  css/              tokens.css, components.css, layout.css (niente stili inline)
  i18n/it.json, en.json
```

1. **Componenti prima delle viste.** Tabella (ordinamento, filtro, paginazione, stato vuoto
   guidato), form con validazione, drawer di dettaglio, conferma, badge di stato con icona + testo.
   Ogni vista migrata usa solo questi.
2. **Rendering sicuro.** Un piccolo helper di template con escape automatico (tagged template
   `html\`...\``) al posto di `innerHTML` + `esc()` manuale. Eventi via `addEventListener` /
   delega, niente `onclick=` in stringa.
3. **Migrazione vista per vista**, partendo dal nucleo salva-vita: Centrale, I miei allarmi,
   Onboarding dispositivo, Lavoratori protetti (nuova, §2.3), poi il resto.
4. **i18n reale.** Ogni stringa delle viste migrate passa da `t()`, con chiavi in `it.json`/`en.json`.
5. **Responsive dalla base.** Layout a griglia con breakpoint su token; le viste del reperibile sono
   progettate prima per il telefono.

**Criteri di accettazione Fase 2:** `app.html` sotto 30 KB; zero `style="` e `onclick=` nelle viste
migrate; ogni vista sotto 600 righe; tutte le stringhe delle viste migrate in i18n; Lighthouse
accessibilità ≥ 90 sulle viste del nucleo.

#### Fase 3. Nuova architettura dell'informazione e UX (3-4 settimane, sovrapponibile alla fine della Fase 2)

##### 3.1 Menu per persona (da 35 a circa 14 voci)

| Sezione | Voce | Sostituisce / fonde | Per chi |
|---|---|---|---|
| **Allarmi** | Centrale | Centrale allarmi + Mappa allarmi + Piantine (safety) come schede Lista / Mappa / Piantina | operatore |
| | I miei allarmi | invariata, versione mobile-first | reperibile, responsabile |
| | Report | invariata | operatore, auditor |
| **Protezione** | Lavoratori protetti | Profili di protezione + Trigger + assegnazione device (vista per persona, §3.2) | admin sicurezza |
| | Dispositivi | Dispositivi & App + onboarding guidato | admin sicurezza |
| | Reperibilità | gruppi, turni on-call, assenze e sostituzioni (da *Escalation e reperibilità*) | admin sicurezza |
| | Regole di allarme | routing eventi + notifiche + gruppo predefinito | admin sicurezza |
| **Campo** | Interventi | Interventi + Richieste (scheda) | pianificatore, tecnico |
| | Cantieri | Cantieri + Mappa cantieri (scheda) | pianificatore |
| | Ronde | Ronde + Cruscotto vigilanza (scheda) | vigilanza |
| | Pianificazione | Calendario + Turni + Agenda tecnici (un calendario con filtri per tipo) | pianificatore |
| | Consuntivi | invariata | lavoratore, responsabile |
| | Anagrafiche commerciali | Clienti + Listino + Contratti (schede) | pianificatore |
| **IoT** | Monitoraggio | Monitoraggio + Alert IoT + cruscotti scenario (schede) | operatore |
| **Impostazioni** | (una sola voce, hub interno) | Configurazione guidata, Utenti e ruoli, Utenti e sedi, Moduli, AI, Privacy e audit | admin |
| | Assistente AI | Agente AI (l'uso); la configurazione va in Impostazioni | tutti |

"Connettori" sparisce. La **diagnostica** (motore escalation, heartbeat, ingress email, telemetria
grezza, comando GPRS) va in *Impostazioni → Diagnostica*, visibile solo attivando la **modalità
avanzata**.

Una sola **mappa** riusabile con livelli (persone, dispositivi, allarmi, cantieri, geofence) e
filtri: le quattro mappe di oggi diventano quattro "viste predefinite" dello stesso componente.

##### 3.2 "Lavoratori protetti": il centro del prodotto

Una tabella delle persone con lo stato di protezione leggibile a colpo d'occhio:

| Lavoratore | Dispositivo | Protezioni attive | Chi interviene | Ultimo segnale | Stato |
|---|---|---|---|---|---|
| Mario Rossi | GH-5200 · batteria 82% | SOS, caduta, immobilità | Squadra Nord (reperibile ora: L. Bianchi) | 2 min fa | ✅ Protetto |
| Anna Verdi | nessuno | solo check-in app | Squadra Nord | 3 h fa | ⚠️ Protezione parziale |

Il dettaglio di una riga mostra la catena completa in linguaggio naturale ("Se Mario cade, entro
30 secondi viene avvisato Luca Bianchi per SMS e chiamata; se non risponde entro 2 minuti, Paola
Neri") e permette di modificarla in un punto solo. Il motore resta quello attuale: cambia solo il
modo di presentarlo.

##### 3.3 Percorso di onboarding unico (obiettivo: lavoratore protetto in meno di 10 minuti)

1. **Scansiona** il dispositivo (QR/barcode, scanner v3 o telefono via PWA).
2. **Assegna** a un lavoratore (ricerca per nome).
3. **Configura** il dispositivo: guida per modello (per il GH-5200: server e porta da impostare).
4. **Prova**: il sistema attende il primo segnale e chiede di premere SOS; mostra in tempo reale
   "segnale ricevuto → allarme creato → reperibile avvisato".
5. **Conferma**: il lavoratore compare in *Lavoratori protetti* come ✅.

Ogni passo ha un controllo di stato e un messaggio comprensibile in caso di problema ("Il
dispositivo non si è ancora collegato: verifica la SIM e l'indirizzo server").

##### 3.4 Allarme sempre in primo piano

- Banner rosso persistente in cima a ogni pagina, per ogni ruolo che deve reagire, finché esiste un
  allarme critico non preso in carico.
- Presa in carico in 2 tocchi da qualsiasi schermata e dalla notifica.
- Livelli di gravità con colore **e** icona **e** testo (mai solo colore).

##### 3.5 Linguaggio

Un **glossario** di prodotto (`docs/glossario.md`) e una revisione dei testi:

| Oggi | Domani |
|---|---|
| Gruppo di default (catch-all) | Squadra predefinita (riceve gli allarmi non assegnati) |
| Routing eventi → gruppo | Chi riceve quale allarme |
| Heartbeat monitor | Controllo "sistema vivo" |
| Ingress email → allarme | Allarmi da email |
| Token di ingest | Chiave di collegamento del dispositivo |
| Traccar / sorgente traccar | Dispositivo GPS |
| Profilo di protezione | Protezioni del lavoratore |

##### 3.6 Verifica con utenti reali

Per ogni percorso del nucleo (onboarding, presa in carico allarme, configurazione reperibilità) una
prova con 3-5 utenti reali, misurando tempo, errori e richieste di aiuto. I percorsi falliti tornano
in progettazione prima di andare avanti.

**Criteri di accettazione Fase 3:** menu ≤ 15 voci per il ruolo più ampio; nessun termine del
glossario "vietato" in UI (controllo automatico in CI); onboarding GH-5200 → lavoratore protetto
< 10 minuti per un admin alla prima esperienza; presa in carico allarme ≤ 2 tocchi su telefono.

#### Fase 4. Consolidamento (1-2 settimane)

- **Manuale unico** (`docs/manuale.md`) organizzato per persona, generato dalla nuova struttura;
  archiviazione dei manuali "parity" e dei piani superati in `docs/archivio/`.
- **Responsive** completato e **traduzione EN** delle viste migrate.
- **Guardrail CI da avviso a bloccanti.**
- **Processo per le nuove funzioni:** scheda breve con persona, percorso, voce di menu (nuova o
  esistente), cosa si toglie o si fonde, metrica di successo. Senza scheda la funzione non entra.

### 2.3 Calendario indicativo

| Settimana | Lavoro |
|---|---|
| 1 | Fase 0 completa; spike `TenantCtx` + `TenantDb`; spike struttura moduli ES |
| 2-3 | Fase 1 split backend + rimozione connettori; Fase 2 componenti UI |
| 4 | Fase 1 errori/test/seed; Fase 2 migrazione viste del nucleo |
| 5-6 | Fase 3: menu nuovo, Lavoratori protetti, percorso onboarding, banner allarme |
| 7 | Fase 2/3 viste Campo e IoT; prove con utenti |
| 8 | Fase 4: manuale, responsive, EN, guardrail bloccanti |

### 2.4 Decisioni del product owner (23/11/2026)

1. **Connettori esterni: rimozione completa**, inclusi riferimenti nel DB, nei file indice e nella UI,
   con test di regressione. *Stato: fatto (Step 1, vedi §2.7).*
2. **Scenari IoT: restano** come scenari attivabili del modulo **IoT Dashboard**, ciascuno con i
   propri dispositivi IoT, e devono essere **gestibili completamente da YouWorkforce** (oggi il
   catalogo è nel codice). *Stato: da fare (Step 2).*
3. **Freeze funzionale accettato**, a condizione che non ci siano problemi: codice moderno e
   scalabile, governabile, usabilità secondo gli standard attuali, tutto verificato.
4. **Utenti di prova per tutti i ruoli**, dal superadmin al lavoratore, per eseguire tutte le prove.
   *Stato: da fare (Step 3).*

### 2.5 Metriche di successo

| Metrica | Oggi | Obiettivo |
|---|---|---|
| File sorgente più grande | 911 KB (`api.rs`) | < 1.500 righe |
| `app.html` | 578 KB | < 30 KB (guscio) |
| Voci di menu (tenant admin) | 35 + dinamiche | ≤ 15 |
| `owner_pool` negli handler tenant | 387 usi nel file | 0 |
| `unwrap()` negli handler | 363 | 0 |
| Stili inline | 1.153 | 0 |
| Test unitari sul dominio | 0 | regole critiche coperte |
| Onboarding → lavoratore protetto | non misurato | < 10 min |
| Presa in carico allarme su telefono | non misurato | ≤ 2 tocchi |
| Righe di codice totali | baseline Fase 0 | almeno −25% a funzionalità del nucleo invariata |

### 2.6 Rischi e mitigazioni

| Rischio | Mitigazione |
|---|---|
| Regressioni durante lo split | spostamento meccanico a comportamento invariato, un dominio per PR, `qa/run-all.sh` verde obbligatorio |
| Il freeze rallenta le vendite | il nucleo salva-vita resta modificabile; le demo usano il ramo stabile |
| Utenti abituati al menu attuale | redirect dalle vecchie rotte alle nuove; nota "dove è finito" per due rilasci |
| Rimozione di funzioni ancora usate | log di utilizzo delle route per 1-2 settimane prima della rimozione |
| Moduli ES senza bundler lenti al primo caricamento | HTTP/2 dalla console + service worker già presente; precaricamento delle viste del nucleo |

### 2.7 Avanzamento

**Step 1 — Rimozione dei connettori esterni (fatto).**

- Backend: eliminate ~570 righe di proxy verso il server Traccar (connessione, poller, verifica,
  report, conducenti, manutenzioni, attributi calcolati, accumulatori, registrazione device, comandi
  via API esterna) e la modalità "motore di escalation esterno".
- Sostituzioni native dove la funzione serviva davvero:
  - **watchdog del silenzio** dei dispositivi assegnati (prima esisteva solo dentro il poller
    esterno): device muto oltre soglia → incidente `perdita_contatto` (salva-vita);
  - **ultimo contatto** registrato a ogni pacchetto, anche senza fix GPS;
  - **comandi rapidi** sulla connessione Teltonika nativa, con storico ed esito chiaro;
  - **report percorsi/soste/riepilogo** calcolati da YouWorkforce (`track.rs`, con unit test).
- Il modulo `traccar` diventa **`gps` — Localizzazione GPS e badge**; sorgente dei device `gps`;
  ingest JSON su `/api/ingress/gps`. Migrazioni `0125` e `0126` (tabelle/colonne del server esterno
  eliminate, dati rinominati). Le migrazioni storiche restano invariate: sono immutabili per
  costruzione (checksum) e documentano la storia dello schema.
- Corretto un bug latente: salvare la configurazione del modulo cancellava la policy eventi.
- Console: pagina **Collegamento dispositivi** (al posto di "Connettori") con stato dei dispositivi,
  soglia di silenzio, policy eventi e chiavi di collegamento; nessun termine "Traccar" in UI.
- Test: rimosse 11 suite dei connettori, aggiornate quelle dei canali nativi, nuove
  `gps-native-e2e.sh` e `crypto-e2e.sh` (chiave AI cifrata); CI con unit test e senza job "motori".
- Documentazione: README riscritto, manuale dispositivi `dispositivi-gps.md`, documenti storici
  spostati in `docs/archivio/`.

**Step 1b — Correzioni dalla code review di b904493 (fatto, commit `fbe89eb`).**

La review ha trovato 9 rilievi, tutti verificati (7 confermati, 2 parziali) più un difetto
preesistente di isolamento. Correzioni:

- **Isolamento tenant sul bridge Teltonika** (bloccante): l'IMEI identifica un solo device GPS in
  tutto il sistema; un IMEI conteso tra tenant è rifiutato (mai `LIMIT 1`). Connessioni registrate
  per (tenant, IMEI): i tre percorsi di invio (comandi rapidi, GPRS grezzo, RPC IoT) raggiungono
  solo il socket del proprio tenant. Comandi serializzati per connessione. Testo libero e RPC verso
  device GPS richiedono `devices.admin`. Trigger DB contro nuovi IMEI GPS duplicati tra tenant.
- **Ingest JSON** accettato con qualunque modulo dispositivi (prima solo con `iot`).
- **Presenza OsmAnd** registrata a ogni pacchetto autenticato, anche senza coordinate.
- **Watchdog**: misura l'ultimo contatto da `GREATEST(last_seen_at, silence_armed_at)`; armamento
  all'upgrade e a ogni assegnazione → niente allarmi a raffica all'upgrade o alla consegna.
- **Migrazione 0127** (0125/0126 intatte): armamento, vincolo identità GPS, chiusura degli incidenti
  `traccar_conn_down` aperti (escalation ferma), passaggio dal motore esterno: scala riavviata solo
  dove nessuno era stato avvisato (consegne "in ombra" archiviate in timeline), stato intatto dove
  la consegna era reale; gli incidenti accettati da GoAlert senza stato nativo sono annotati per
  revisione. Gli incidenti `perdita_contatto` già aperti non si toccano.
- **Configurazione strumenti**: il form possiede i campi dello schema (omesso → default; un campo
  vuoto non diventa più `0`), le chiavi gestite altrove restano, la risposta è il valore salvato.
- **Report**: oltre 50.000 posizioni il risultato è dichiarato parziale (header, campi, CSV, avviso
  in console). **Scheda dispositivo**: "in linea" con la soglia reale del tenant.
- Test: `gps-isolation-e2e.sh` (36), `upgrade-0127-check.sh` (21), 7 nuovi unit test.

**Step 1c — Verifica finale e correzioni (fatto, commit `fbe89eb`).**

- **Stato di servizio esplicito dei badge** (mig 0128): l'assegnazione non vale più come messa in
  servizio. Il conteggio del silenzio parte dalla *messa in servizio* (azione esplicita) e riparte
  al passaggio di consegne; un badge in servizio mai collegato fa scattare l'allarme alla soglia.
  In attesa / fuori servizio: visibili, nessun allarme. Chi era monitorato conserva il monitoraggio.
- **Titolarità GPS** in `app.gps_identity` (chiave primaria): unicità valida con qualunque livello
  di isolamento; rilascio alla cancellazione; contesi marcati.
- **Bridge Teltonika**: ogni connessione ricorda il device; titolarità ricontrollata a ogni
  pacchetto (chiusura se cambiata); gli invii usano solo la connessione del device corrente.
- **Controlli in sola lettura** per gli ambienti di destinazione: `qa/sql/check-imei-contesi.sql`,
  `qa/sql/check-escalation-0127.sql`, `qa/sql/check-badge-0128.sql` (stato dopo 0128 e azione per il
  responsabile operativo). Scheda di collaudo staging: `docs/collaudo-staging-gps.md`.

**Dal 24 al 26/11 — registro errori e decisioni del PO (fatto, pubblicato su GitHub il 26/11).**

Il lavoro è proseguito sul registro «Stato dello sviluppo e registro errori» (123 errori in 7 aree:
superadmin, tenant, sicurezza lavoratore, lavoro sul campo, IoT/AI, console, architettura). Il
registro ha una sezione «Avanzamento al 26/11/2026» con stato e commit di ogni errore: **121
corretti, 2 in parte** (TA-16 `iam-refactor-plan.md` da riallineare; SL-21 commenti del codice di
escalation). Ultimo commit `f508fa3`, migrazioni fino alla `0164`. In sintesi:

- **Fase 1 (struttura)**: `api.rs` diviso in moduli (`superadmin`, `devices`, `iot`, `field`,
  `oncall`, `ai`, `catalogs`); controlli statici in regressione e CI (`handler-guards`,
  `owner-pool-audit`, `console-syntax`, `glossary-check`); trigger `yw_same_tenant` su 88 tabelle.
- **Sicurezza**: rete di ultima istanza per gli allarmi senza destinatari, immobilità configurabile,
  SOS non duplicati, verifica in due passaggi obbligatoria per il superadmin, segreti cifrati con
  chiave sigillata dal server, limiti di accesso per account e IP.
- **Permessi**: deleghe controllate (non si concede ciò che non si ha), menu e widget per
  permesso, nuovi permessi `sites.manage` e `work.own`, inviti con link monouso.
- **Prodotto**: pacchetti e tipi di evento in database gestiti dal superadmin, un solo diario,
  manutenzioni ricorrenti automatiche, ore lavorate, listino in centesimi con qualifiche, scenari
  IoT con telemetria e comandi veri, ronde mancate come allarmi.
- **Console (Fase 2 e 4)**: menu ridotto a ~27 voci con schede, accessibilità e telefono,
  glossario unico, **interfaccia inglese completa** (`console/static/i18n.js` + dizionario
  generato, vedi `console/tools/i18n/README.md`; supera il piano `it.json`/`en.json` per vista
  descritto sopra).
- **Regressione**: `qa/run-all.sh` 137/138 all'ultimo giro completo; il fallimento era un bug vero
  (piano di ronda «dalle = alle»), corretto in `f508fa3`.

Resta: prova reale con il badge GH-5200, build di release e deploy, rilettura madrelingua delle
traduzioni.
