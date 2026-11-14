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

### 3. Connector — integrazioni esterne (attivabili dal superadmin)

`kind="connector"`. Gated da `require_module(code)`.

| Connettore | code | Dipende da | Note |
|------------|------|-----------|------|
| GoAlert (escalation) | `goalert` | loneworker | motore on-call; il nativo funziona anche senza |
| Traccar (badge/GPS) | `traccar` | loneworker | posizioni/eventi → incidenti |
| IoT generico | `iot` | — | telemetria + regole sensori |
| ThingsBoard | `thingsboard` | — | pull telemetria/allarmi nativi |

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
| Identità/utenti | `api.rs` | da estrarre (deps login/sessione) |

I moduli product/connector e il resto dei servizi vivono ancora in `api.rs` e migreranno con lo
stesso pattern quando utile. ThingsBoard è il prossimo candidato (vedi sotto).

## Nota per ThingsBoard (prossimo lavoro)

Il codice TB vive oggi in `api.rs` (~102 funzioni, cluster ~1162–1413 e ~9513–9680). Fase B3 lo
estrae in `core/src/thingsboard.rs` mantenendo il seam: inventario device via `core.devices`,
pipeline allarmi via il modulo `thingsboard`. Vedi `moduli-autonomia.md` e `thingsboard-manuale.md`.
