# YouWorkforce — Progetto di Porting dei Moduli (PLAN ONLY)

> Scritto per: il team di sviluppo/architettura. Obiettivo: rendere **ogni modulo un'unità pulita,
> self-contained, di prima classe, attivabile/deployabile in modo indipendente**. Questo è un
> piano: non modifica codice. (Prodotto da analisi multi-agente sul codice reale.)

## 0. Come è cablato oggi il sistema (la baseline che il piano deve cambiare)

Cinque fatti alla radice di ogni gap di portabilità:

1. **Il gating è runtime-per-handler, non strutturale.** Ogni route in `main.rs` è registrata
   incondizionatamente in un unico `Router`. "Modulo spento" è imposto solo da chiamate sparse a
   `module_active()`/`require_module()`/`require_any_module()` dentro gli handler, con **stringhe
   hardcoded** ("loneworker", "traccar", …). ~40 call site.
2. **`set_module` non ha lifecycle.** Valida lo stato, fa `UPDATE tenant_modules.state`, scrive audit.
   Unica regola: blocca la disattivazione di `loneworker` con incidenti aperti. Nessun auto-provision,
   teardown, reconcile, health.
3. **Migrazioni = catena lineare piatta**, possedute dal ruolo owner, eseguite da `sqlx::migrate!`.
   Lo schema di un modulo è **spalmato su molti file** (Traccar: 0012–0036). Nessun modulo possiede il suo DDL.
4. **Due data plane.** Tabelle operative tenant (~38) con RLS `tenant_isolation`. Tabelle
   connettori/segreti/poll-state (`*_connections`, `goalert_services`, `ai_connections`, `ai_features`,
   `*_poll_state`, `goalert_routing`) **senza RLS**, via `owner_pool` con filtro `tenant_id` esplicito;
   segreti `enc:v1:` a riposo.
5. **Un'unica pipeline condivisa ingest→incident→escalation.** `persist_ingest` + `eval_sensor_rules`
   + l'insieme `CRITICAL` alzano `incidents`, scrivono `incident_timeline`, accodano
   `outbox('goalert_notify')` in una tx; il worker drena l'outbox→GoAlert e fa `traccar_poll`/
   `thingsboard_poll` cross-tenant su `owner_pool`. È il tessuto connettivo che accoppia
   traccar/iot/thingsboard/loneworker → goalert.

Superficie di config (buona base, da generalizzare): `module_tools` (catalogo globale,
`config_schema`+`default_config`), `tenant_tool_enabled`, `tenant_module_config`, sovrapposti da
`tool_config()`; più `ai_features` (toggle AI per-processo).

## 1. Analisi per modulo (la table ownership sotto è QUELLA PROPOSTA)

### loneworker — Lone worker + centrale allarmi (Effort: L)
Sicurezza field: check-in timer, SOS, presenza turno, console incidenti/centrale. Possiede (proposto):
`checkins`, `shifts`, `operator_sites`, cluster alarm-routing (`alarm_groups`, `alarm_group_members`,
`alarm_routing`); condivide la catena incidenti core. **Dipendenza dura da `goalert`**. Hook AI:
`checkin_ai_anomaly`, `incident_ai_summary`. Gap: la console legge incidenti prodotti da ALTRI moduli →
non è davvero standalone (è il *consumer* della pipeline); SOS hardcoda GoAlert via outbox; overlap
alarm-routing con `workforce`; nessun teardown. Piano: tenere `events`/`incidents`/`outbox` come
**core platform** (non loneworker-owned) e far *sottoscrivere* loneworker all'evento incidente; dep
goalert esplicita nel manifest; lifecycle con drain timer + invariante "no disable con incidenti aperti".

### goalert — connettore escalation (Effort: M)
Unico motore escalation. Possiede `goalert_services` (enc), `goalert_routing`, `oncall_rotation`,
`user_phone`. Tutto il critico passa da `outbox('goalert_notify')`. Gap: contratto implicito (le stringhe
kind dell'outbox sono l'interfaccia di fatto); segreti non-RLS; nessuna interfaccia "escalation provider"
formale. Piano: promuovere i kind a **interfaccia EscalationProvider** (trait Rust `notify/close/ack/status`,
GoAlert unico impl); i chiamanti accodano richieste provider-agnostiche; manifest `provides: escalation`.

### traccar — connettore GPS badge (Effort: L)
Superficie più grande. Possiede `traccar_connections`, `traccar_poll_state`, `geofences`,
`geofence_devices`, `geofence_events`, `device_positions`, `device_presence`, `presence_log`,
`position_filters`, `geocode_cache`, `beacons`, `floorplans`, `device_floorplan`, `map_views`,
`device_commands`. Watchdog alza `traccar_conn_down`. Gap: schema spalmato su 12+ migrazioni; possiede
molto che potrebbe essere sub-modulo (indoor/beacons/floorplans; geofencing; reporting); `ui_features`
accoppia il menu a una tool-config traccar anche per tenant non-traccar. Piano: consolidare il DDL in un
namespace; ingest come **DeviceSource provider** che emette `Normalized`; registrazione poll nell'hook
`reconcile` del modulo.

### iot — dispositivi e IoT generico (Effort: M)
Ingest HTTP generico + **motore regole sensori** parametrico; backbone degli scenari. Possiede
`sensor_rules`, `tenant_scenarios` + catalogo scenari (codice) + spec runtime (`SCENARIO_SPECS_DIR`).
`eval_sensor_rules` → etype → pipeline condivisa. Gap: il motore regole è *infrastruttura core* usata da
più connettori ma nominalmente "iot"; catalogo scenari in codice; confine debole vs thingsboard. Piano:
estrarre **ingest+valutazione regole in un servizio "telemetry pipeline" core**; `iot` resta la source
HTTP generica + owner UI/CRUD regole; scenari come feature iot con teardown già reversibile (`scn:` label).

### thingsboard — connettore IoT industriale (Effort: M)
Connettore a ThingsBoard esterno (push rule-engine + pull background). Possiede `thingsboard_connections`,
`thingsboard_poll_state`. Pull/push convergono su `eval_sensor_rules`/`persist_ingest`. `requires: iot`
(solo convenzione). Piano: stesso pattern connettore di traccar (DeviceSource provider + tabella segreti
standard + `reconcile` registra il poll + health = verify); `requires: [iot]` enforced all'enable.

### workforce — forza lavoro e turni (Effort: S)
Teams, shift plan, reperibilità (assegnazione manuale). Possiede `teams`, `team_members`, `shift_plans`,
`oncall`. Overlap concettuale della reperibilità tra workforce, loneworker (`alarm_routing`), goalert
(`oncall_rotation`). Piano: chiarire ownership — workforce possiede la *pianificazione*; goalert il
*routing escalation*; interfaccia read sottile "chi è on adesso".

### interventi — ordini di lavoro (Effort: S)
Possiede `work_orders`. Hook AI `work_order_ai_assist` + tool agente. Quasi modulo di riferimento: pulito,
condivide solo `users`/`sites`. Piano: namespace `work_orders`, dichiarare hook AI, lifecycle enable/disable
con storico conservato.

### consuntivi — ore, spese, consuntivi (Effort: S)
Possiede `timesheets`, `timesheet_lines`, `expenses`, `timesheet_events`. Config `consuntivi/rates`. Hook
`consuntivo_ai_summary` (nota: tensione col contratto "no AI" come *fonte dati*). Modulo di riferimento,
self-contained.

### assistant (AI) — attore AI multi-provider (Effort: L, meccanico)
Attore AI cross-process + agente tool-calling che legge/scrive DB con permessi UI. Possiede
`ai_connections` (enc), `ai_features`, `ai_reports`, `diary_entries`. **Trasversale a ogni modulo** (i tool
dell'agente controllano `module_active` di ogni target). Gap: gli hook AI sono *chiamati da* handler dei
moduli → dipendenza bidirezionale; `ai_features` è un enum hardcoded accoppiato a moduli specifici. Piano:
ri-inquadrare assistant come **capability con registry di hook**: ogni modulo *dichiara* i suoi hook AI nel
manifest; assistant fornisce runtime provider/agente ed enumera gli hook dai moduli registrati (non enum
hardcoded). Disattivare assistant deve lasciare intatti i flussi manuali (ogni hook opzionale al call site).

### Sub-feature (non moduli di catalogo) — nota ownership
- **scenari/ambienti** → iot (reversibile, catalog-driven).
- **geofence** → traccar.
- **indoor (beacons/floorplans/map_views)** → oggi traccar; candidato sub-modulo.
- **device catalog/provisioning** (`device_models`, `assets`, `device_provisioning`) → **core platform**
  condiviso (usato da `DEVICE_MODS`/`CONNECTOR_MODS`), non di un singolo modulo.

## 2. CROSS-CUTTING: il MODULE CONTRACT proposto

### 2.1 Manifest di modulo (dichiarativo, uno per modulo)
Descrittore statico (struct/const Rust registrato all'avvio, con riga in `modules_catalog`) che sostituisce
le stringhe hardcoded:
```
ModuleManifest { code, name, kind: product|connector, available,
  requires: [capability], provides: [capability],
  owns_tables: [...], routes: fn(Router)->Router,
  config_tools: [module_tools rows], ai_hooks: [{feature, endpoint, write}],
  rls_tables: [...], lifecycle: { install, enable, disable, uninstall, reconcile, health } }
```

### 2.2 Lifecycle hooks (cuore del retrofit) — `set_module` diventa un orchestratore
| Hook | Quando | Comportamento |
|---|---|---|
| install | modulo appare nel catalogo (deploy) | crea gli oggetti schema posseduti (idempotente), tenant-agnostic |
| enable(tenant) | superadmin → attivo/da_configurare | **auto-provision**: seed default, crea righe connettore `da_configurare`, registra provider/poll, enforce `requires`. SM: `non_abilitato → da_configurare → attivo`/`degradato` |
| disable(tenant) | → in_disattivazione/non_abilitato | **teardown**: ferma job/poll, drena in-flight, **rifiuta se invariante** (loneworker+incidenti aperti), storico leggibile ai ruoli autorizzati |
| uninstall(tenant) | rimozione hard | drop/anonimizza per retention (opt-in, audited) |
| reconcile(tenant) | periodico/on-config | converge desiderato↔attuale (registra poll, ri-provisiona GoAlert, re-sync recapiti) |
| health(tenant) | endpoint stato | probe upstream → mappa su `attivo`/`degradato` |

Così i 5 stati UI (`non_abilitato · da_configurare · attivo · degradato · in_disattivazione`) hanno un
motore reale dietro (oggi sono solo stringhe).

### 2.3 Registrazione route
Sostituire il `Router` monolitico con **`routes(Router)->Router` per-modulo** composte all'avvio. Il
gating passa da `require_module` per-handler a un **middleware module-scoped** che legge il manifest una
volta per gruppo → elimina ~40 guard hardcoded. I check di ruolo per-handler restano.

### 2.4 Config schema e segreti
Mantenere `module_tools`/`tenant_tool_enabled`/`tenant_module_config`. Ogni modulo **seed-a le proprie
tool-row via manifest** (non più l'INSERT monolitico 0008). Standardizzare i segreti connettore in un
pattern **`ConnectorSecret`** uniforme `{base_url, credentials(enc), verify(), status}` con una sola
sweep `crypto_migrate`.

### 2.5 Servizi core (disaccoppiare la pipeline condivisa)
Estrarre tre seam dietro interfacce: `TelemetryPipeline::ingest(Normalized)` (wrap `persist_ingest`),
`RuleEngine::eval(payload)` (wrap `eval_sensor_rules`, owner core, CRUD/UI iot), `EscalationProvider`
trait (unico path da `incidents` al paging; outbox kind provider-agnostici).

### 2.6 RLS, AI hook, health/reconcile
`rls_tables` di ogni modulo ottengono `tenant_isolation` via macro/DO-block chiamato dalla migrazione del
modulo. Moduli dichiarano gli hook AI; assistant consuma un **registry** (non enum). Un
`GET /api/modules/:code/health` espone `health()`; il `tick` del worker chiama `reconcile()` di ogni modulo
attivo invece degli join hardcoded.

## 3. Roadmap di retrofit per fasi
- **Fase 0/1a (manifest) — ✅ FATTO** (`core/src/modules.rs`): `Manifest{code,name,requires,provides,
  description}` + `REGISTRY` (11 moduli) + `missing_deps`/`dependents_active`; esposto in
  `super_list_modules` (requires/provides/description) e validazione **soft** (avvisi, non bloccano) in
  `set_module` all'abilitazione/disabilitazione. 5 test unit verdi. Nessun cambio di comportamento
  (gating esistente invariato). Restano: trait lifecycle (install/enable/disable/health) + middleware
  module-scoped + estrazione servizi.
- **Fase 0** — Scaffolding contratto (nessun cambio comportamento): `ModuleManifest` + trait lifecycle +
  registry; route/guard attuali in parallelo. *M, rischio basso.* (manifest fatto; trait/lifecycle da fare)
- **Fase 1** — Estrazione servizi core (`TelemetryPipeline`/`RuleEngine`/`EscalationProvider`), refactor puro;
  bloccata dai QA esistenti. *L, rischio medio.*
- **Fase 2** — Moduli di riferimento prima: `consuntivi`, `interventi`, `workforce` (S ciascuno). *M, basso.*
- **Fase 3** — Connettori: `goalert`, `thingsboard`, poi `traccar` (il più grande). *L, medio.*
- **Fase 4** — Prodotti sulla pipeline: `iot` (split rules/scenari), poi `loneworker` (invarianti, teardown). *L, alto.*
- **Fase 5** — Assistant come capability: `ai_features` enum → hook registry. *L, medio (meccanico).*
- **Fase 6** — Decommissionare il gating legacy: rimuovere le guard stringa, i seed monolitici, assorbire il
  blocco route in `routes()` dei moduli. *M, basso.*

### Rischi trasversali
- loneworker↔console: incidenti prodotti da altri moduli; disabilitare moduli device non deve orfanare la
  console → tenere `events`/`incidents`/`outbox` come core.
- `devices`/`sites`/catalog condivisi → infrastruttura core, non di un modulo.
- invariante "unico motore" GoAlert → `EscalationProvider` single-impl per policy.
- teardown/retention → disable conserva lo storico; solo `uninstall` rimuove dati, sempre audited.
