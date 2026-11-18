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

### 3. Connector — RITIRATI (suite self-contained)

YouWorkforce **non si connette mai a servizi esterni**. I connettori-ponte verso server esterni
(`goalert`, `traccar`, `thingsboard`) sono **ritirati dal prodotto** (mig 0078: `available=false`,
disattivati). Ogni loro funzione ha un equivalente **nativo**:

| Ex connettore | Sostituito da (nativo) |
|---------------|------------------------|
| GoAlert (on-call/escalation) | **escalation nativa** (default, mig 0057) |
| Traccar (badge/GPS, server esterno) | **edge nativo** Teltonika Codec8/8E + ingest OsmAnd/HTTP |
| ThingsBoard (pull telemetria esterna) | **IoT nativo**: `iot` (sensor rules) + scenari "IoT Use Cases" |

**Ingest INBOUND resta nativo**: gli endpoint `/api/ingress/osmand|traccar|thingsboard` RICEVONO
telemetria (device che spingono verso YW, non YW che chiama fuori) e sono gated dal modulo **`iot`**
(native) via `DEVICE_MODS = [loneworker, iot]`. Il formato del payload (OsmAnd/Traccar/ThingsBoard)
è solo un'etichetta di sorgente; il motore è YW.

L'unica integrazione esterna rimasta, **opzionale**, sono i **provider AI** (richiedono la chiave del
cliente): vedi modulo `assistant`.

## Chi attiva cosa (separazione dei poteri)

- **Moduli** (product+connector) on/off → **solo superadmin** (`require_super`, tabella
  `tenant_modules`). Il tenant_admin NON può attivarli.
- **Capability/ruoli, layout dashboard, config** → **tenant_admin** (capability dedicate).
- **Core** → non si attiva: esiste sempre.

## Perimetri e autonomia

Due perimetri di prodotto, entrambi autonomi e coesistenti (vedi `moduli-autonomia.md`):

- **Sicurezza lavoratore** = loneworker + goalert/traccar/iot/thingsboard.
- **Lavoro sul campo** = interventi/giri/consuntivi/workforce.

Regole d'oro:
1. Nessun modulo di un perimetro dipende funzionalmente da un modulo dell'altro.
2. I servizi **core** sono gli UNICI punti di condivisione legittimi tra moduli (utenti, gruppi,
   sedi, permessi, dispositivi-inventario).
3. L'escalation/centrale è condivisa da tutti i moduli che **producono allarmi**
   (`ESCALATION_MODS = loneworker,traccar,iot,thingsboard`).

## Dispositivi: inventario (core) vs pipeline allarmi (safety)

Punto delicato e rilevante per ThingsBoard:

- **Inventario/config dispositivi** = `core.devices`: censimento, attributi, onboarding, geofence.
  Infrastruttura condivisa.
- **Pipeline allarmi** (evento → incidente → escalation) = appartiene ai moduli safety
  (loneworker/traccar/iot/thingsboard). Un device senza un modulo safety attivo resta inventario,
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

I moduli product/connector e il resto dei servizi vivono ancora in `api.rs` e migreranno con lo
stesso pattern quando utile. ThingsBoard è il prossimo candidato (vedi sotto).

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
  `started_at`, watchdog heartbeat vs `last_ping_at`) e **interazioni con sistemi esterni** (shadow
  GoAlert, annunci Slack, suffissi-ID univoci) restano su `chrono::Utc::now()` (wall-clock), con
  commento esplicito nel codice: mescolare clock di test e `now()` SQL falserebbe gli elapsed.

## Nota per ThingsBoard (prossimo lavoro)

Il codice TB vive oggi in `api.rs` (~102 funzioni, cluster ~1162–1413 e ~9513–9680). Fase B3 lo
estrae in `core/src/thingsboard.rs` mantenendo il seam: inventario device via `core.devices`,
pipeline allarmi via il modulo `thingsboard`. Vedi `moduli-autonomia.md` e `thingsboard-manuale.md`.
