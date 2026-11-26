# Architettura modulare di YouWorkforce

YouWorkforce è un sistema **modulare**: ogni modulo è autonomo e i moduli sono **componibili** sullo
stesso tenant. Sopra i moduli c'è uno strato di **servizi trasversali** (core) che tutti i moduli
condividono. Questo documento è la bussola: cosa è core, cosa è modulo, chi dipende da chi, chi può
attivare cosa. La fonte di verità in codice è `core/src/modules.rs` (`REGISTRY`).

## I tre strati

### 1. Core — servizi trasversali (sempre presenti, mai module-gated)

Non attivabili/disattivabili: ci sono per ogni tenant. Gating solo per **capability** (ruolo), mai
per modulo. Nel registry hanno `kind="core"`.

| Servizio | code | Capability | Note |
|----------|------|-----------|------|
| Utenti e identità | `core.identity` | users.manage | login, ruoli base |
| Gruppi di persone | `core.groups` | users.manage | insiemi riusabili da qualunque modulo |
| Sedi e anagrafiche | `core.sites` | users.manage | sedi, asset/mezzi, squadre |
| Permessi e ruoli | `core.iam` | users.manage, settings.manage | capability layer, ruoli custom, override |
| Privacy e audit | `core.privacy` | gdpr.manage, audit.view | GDPR, ritenzione, registro attività |
| Dashboard | `core.dashboard` | dashboard.manage | dashboard unica configurabile |
| Dispositivi (inventario) | `core.devices` | devices.admin, devices.manage | **inventario/config** condiviso; la pipeline allarmi è dei moduli safety |

Endpoint core NON chiamano `require_module`; usano solo `require_tenant` + `require_cap`.

### 2. Product — moduli funzionali (attivabili per-tenant dal superadmin)

`kind="product"`. Gated da `require_module(code)`.

| Modulo | code | Perimetro | Widget dashboard |
|--------|------|-----------|------------------|
| Lone Worker e centrale | `loneworker` | Sicurezza | incidents, checkins |
| Interventi e cantieri | `interventi` | Lavoro sul campo | orders |
| Ore, spese, consuntivi | `consuntivi` | Lavoro sul campo | timesheets |
| Ronde e punzonature | `giri` | Lavoro sul campo | rounds |
| Forza lavoro e turni | `workforce` | Lavoro sul campo | shifts |
| Assistente YouWorker.ai | `assistant` | Trasversale (AI attore) | — |
| Qualifiche / Prevenzione | `qualifiche`,`prevenzione` | (non ancora disponibili) | — |

### 3. Nessun connettore esterno (suite self-contained)

YouWorkforce **non si connette mai a servizi esterni**. Le vecchie integrazioni con server esterni
(escalation, tracciamento GPS, piattaforma IoT) sono state rimosse dal prodotto (mig `0078`, codice e
dati residui eliminati con mig `0125`). Ogni funzione è nativa:

| Funzione | Motore nativo |
|----------|---------------|
| Reperibilità ed escalation | **escalation nativa** (`escalation.rs`, `delivery.rs`) |
| Dispositivi GPS e badge | modulo **`gps`**: edge Teltonika Codec8/8E (TCP), ingest OsmAnd, ingest JSON HTTP, watchdog del silenzio, report percorsi/soste (`track.rs`) |
| Telemetria e sensori | modulo **`iot`**: regole e soglie, monitoraggio, scenari attivabili (IoT Dashboard) |

**Ingest INBOUND**: gli endpoint `/api/ingress/osmand` e `/api/ingress/gps` (+ `/position`) RICEVONO
dati dai dispositivi (sono i device a spingere verso YW, mai YW a chiamare fuori). Il tenant deriva
dalla chiave di collegamento, mai dal contenuto.

Le integrazioni esterne, tutte **opzionali**, sono il **provider AI** scelto dal cliente (modulo `assistant`)
e, per il modulo `meteo`, le previsioni di **MET Norway** (fonte scelta per tenant, decisione PO 26/11/2026;
vedi [meteo-operativo.md](meteo-operativo.md)).

## Chi attiva cosa (separazione dei poteri)

- **Moduli** on/off → **solo superadmin** (`require_super`, tabella
  `tenant_modules`). Il tenant_admin NON può attivarli.
- **Capability/ruoli, layout dashboard, config** → **tenant_admin** (capability dedicate).
- **Core** → non si attiva: esiste sempre.

## Perimetri e autonomia

Due perimetri di prodotto, entrambi autonomi e coesistenti (vedi `moduli-autonomia.md`):

- **Sicurezza lavoratore** = loneworker + gps/iot (escalation nativa).
- **Lavoro sul campo** = interventi/giri/consuntivi/workforce.

Regole d'oro:
1. Nessun modulo di un perimetro dipende funzionalmente da un modulo dell'altro.
2. I servizi **core** sono gli UNICI punti di condivisione legittimi tra moduli (utenti, gruppi,
   sedi, permessi, dispositivi-inventario).
3. L'escalation/centrale è condivisa da tutti i moduli che **producono allarmi**
   (`ESCALATION_MODS = loneworker,gps,iot`).

## Dispositivi: inventario (core) vs pipeline allarmi (safety)

- **Inventario/config dispositivi** = `core.devices`: censimento, attributi, onboarding, geofence.
  Infrastruttura condivisa.
- **Pipeline allarmi** (evento → incidente → escalation) = appartiene ai moduli safety
  (loneworker/gps/iot). Un device senza un modulo safety attivo resta inventario,
  non genera allarmi gestibili (Centrale gated `ESCALATION_MODS`).

## Il registry come contratto (SSOT)

`core/src/modules.rs::REGISTRY` descrive ogni modulo/servizio: `code, name, kind, requires,
provides, capabilities, widgets, description`. È consumato da:

- validazione dipendenze all'attivazione (`missing_deps`, `dependents_active`);
- `super_list_modules` (espone kind/capabilities/widgets/requires al pannello superadmin);
- (documentale) mappatura widget↔modulo per la dashboard (il filtro autoritativo resta
  `DASHBOARD_WIDGETS` in api.rs).

Aggiungere un modulo = una voce nel REGISTRY + (se attivabile) una riga in `app.modules_catalog` +
i suoi endpoint gated `require_module`. Aggiungere un servizio core = una voce `kind="core"` (nessun
gating di modulo).

## Stato estrazione codice (moduli Rust)

Modularizzazione per-feature dello strato core (Fase B): i servizi core hanno ciascuno il proprio
file sotto `core/src/`, invece di stare tutti in `api.rs`. Gli helper trasversali di `api.rs`
(`R`, `err`, `ise`, `require_tenant`, `require_cap`, `has_cap`, `is_planner`, `audit`,
`CAPABILITIES`, `USER_ROLES`, `default_role_caps`, `effective_caps`) sono `pub(crate)` e riusati dai
moduli-feature.

| Servizio core | File | Stato |
|---------------|------|-------|
| Dashboard | `core/src/dashboard.rs` | estratto |
| Privacy e audit | `core/src/privacy.rs` | estratto |
| Gruppi di persone | `core/src/groups.rs` | estratto (l'import→gruppo allarme resta in api.rs = bridge escalation) |
| Sedi e asset | `core/src/sites.rs` | estratto (le squadre restano in api.rs = primitiva workforce) |
| Permessi e ruoli (IAM) | `core/src/iam.rs` | estratto (fondamenta login/sessione restano in api.rs) |
| Identità/utenti | `core/src/users.rs` | estratto (CRUD anagrafica + ACL sedi; login/sessione/impersonazione restano in api.rs) |

**Split di `api.rs` (25/11/2026, punto 3).** `api.rs` è passato da ~14.800 a ~3.000 righe: i
moduli di prodotto sono in file propri, spostati **senza cambiare comportamento** (stesse funzioni,
stesse rotte; `api.rs` li riesporta con `pub(crate) use crate::<modulo>::*`, quindi `main.rs` e i
richiami `api::nome` restano validi; gli elementi di primo livello sono `pub(crate)`):

| Area | File |
|------|------|
| Piano di controllo (superadmin): tenant, moduli, monitoraggio, chiave dei segreti, canali di consegna, account e MFA, impersonazione | `core/src/superadmin.rs` |
| Dispositivi e localizzazione: ingest nativo (GPS, OsmAnd, Teltonika, JSON), comandi, geofence, report, mappe, beacon, piantine, catalogo, onboarding | `core/src/devices.rs` |
| IoT nativo: telemetria, attributi, comandi, blueprint, MQTT, alert e monitoraggio, scenari | `core/src/iot.rs` |
| Lavoro sul campo: clienti, interventi, listino, consuntivi, registro ore, ronde, turni, cantieri | `core/src/field.rs` |
| Escalation nativa e reperibilità: impostazioni, recapiti, assenze, manutenzione, calendari, consegna, ack Twilio, servizi | `core/src/oncall.rs` |
| Assistente e processi AI: assistente, processi nei moduli, agente con strumenti, report AI, connessione | `core/src/ai.rs` |

In `api.rs` restano gli helper trasversali (sessione, permessi, errori, audit, rate limit), utenti
e gruppi di allarme/instradamento, centrale e incidenti, SOS e check-in, configurazione dei moduli.
Già estratti prima: report di tracciamento (`track.rs`), Teltonika (`teltonika.rs`), MQTT (`mqtt.rs`).

## Controlli statici delle protezioni (AR-1, AR-2 — 25/11/2026)

I controlli di permesso, modulo e tenant restano nei gestori (difesa in profondità), ma ora sono
**verificati in automatico** a ogni esecuzione di `qa/run-all.sh` (quindi anche in CI), senza core:

- **`qa/handler-guards.sh` (AR-1).** Legge tutte le rotte di `main.rs`, apre il gestore e verifica
  che abbia autenticazione (sessione, token o firma), controllo del modulo (rotte tenant) e
  permesso (scritture tenant). Le eccezioni volute stanno in `qa/handler_guards_allow.txt` con il
  motivo (nucleo indipendente dai moduli, calendari con token, …); un'eccezione per una rotta che
  non esiste più fa fallire il controllo. La prima esecuzione ha trovato **37 gestori senza
  controllo del modulo** (reperibilità classica, annullo e assegnazione degli ordini, approvazione
  dei consuntivi, alert IoT, regole sensori, chiavi di collegamento, impostazioni e report AI, report
  CSV, consegna escalation, immobilità, calendario, allegati di commessa) e un permesso mancante
  sugli **allegati di commessa** (ora fatture e SAL solo a chi vede i costi, foto e diario a chi è nel
  gruppo assegnato): tutti corretti.
- **`qa/owner-pool-audit.sh` (AR-2).** Il pool privilegiato (`owner_pool`) non applica l'RLS:
  l'isolamento dipende dal `tenant_id` scritto nella query. Il controllo trova ogni query su
  `owner_pool` che tocca una tabella con `tenant_id` (ricavate dalle migrazioni) senza filtrarlo.
  Le eccezioni (watchdog di sistema su tutti i tenant, token univoci, id appena letti da una riga
  filtrata) stanno in `qa/owner_pool_allow.txt` con il motivo. Trovata e corretta una lettura per id
  dal percorso senza tenant (impostazione del reperibile attivo di un gruppo).
  Limite: il controllo vede se `tenant_id` compare nella query, non se il filtro è corretto.

Regola per il codice nuovo: una rotta senza controlli o una query privilegiata senza `tenant_id`
non passa la CI finché non è corretta o motivata nell'elenco delle eccezioni.

## Riferimento temporale: orologio unico controllabile

Esiste **un solo** riferimento di tempo applicativo: `st.clock.now()` (`core/src/clock.rs`),
pari a `Utc::now() + offset`. In produzione l'offset è sempre `0` → identico al wall-clock; con
`TEST_CLOCK=1` l'endpoint `POST /api/_test/advance {seconds}` sposta l'offset, rendendo testabili
i percorsi basati sul tempo **senza attese reali**. Regola:

- **Logica/visualizzazione calcolata in Rust dalla configurazione** (slot ronde, rotazioni e
  turni di reperibilità, finestre calendario/iCal, default "oggi" di date utente, e il **motore di
  escalation nativo** — `step_fired_at` è scritto e confrontato con lo stesso `now`, quindi
  auto-coerente) usa `st.clock.now()`.
- **Confronti contro timestamp scritti da `now()` SQL** (throttle login, elapsed ronda/turno vs
  `started_at`, watchdog heartbeat vs `last_ping_at`) e **interazioni con sistemi esterni** (annunci
  Slack, suffissi-ID univoci) restano su `chrono::Utc::now()` (wall-clock), con
  commento esplicito nel codice: mescolare clock di test e `now()` SQL falserebbe gli elapsed.
