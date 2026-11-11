# Progetto: ThingsBoard Parity — chiusura gap YouWorkforce ↔ ThingsBoard

Stato: COMPLETO (e2e 10/10 su TB vivo) · Owner: core/api + console · Motore: ThingsBoard (:8084 dev)

Verifica (globex, TB :8084): alarm policy get/set, sync manuale (tenant admin), inventario,
pull alarm nativo CRITICAL→incidente `sensore_critico` (idem tb-alarm-id), attributi, RPC
(409 offline gestito), shared-attr, storico. Regressione: tb-conn 8, tb-pull 10, sensor-rules 8,
smoke 34. Nota: alarm policy in tool_config `thingsboard/severity_gate` (NON 'alarm_map': manca
il module_tools row → FK; riuso del tool esistente, merge preservando temp_max). Router main
spezzato in più statement `let app = app...` + sub-router merge: una catena .route() unica
troppo lunga fa crashare rustc in codegen (STATUS_STACK_BUFFER_OVERRUN su Windows).
Principio: modulo **completamente gestibile da superadmin e tenant admin**, in modo semplice.
Split netto: **superadmin = connessione** (base/credenziali, cifrate); **tenant admin =
operatività** (regole, device, alarm policy, RPC, attributi, sync). Default sicuri.

## Contesto
Audit doc TB (thingsboard.io/docs/paas/user-guide) vs codice (`tb_pull_once`, `ingest_thingsboard`,
`thingsboard_sync`, `eval_sensor_rules`, AI tool `tb_*`, worker `thingsboard_poll`). Oggi YW:
connessione (superadmin) + PULL (`/api/tenant/devices` specchio + `/values/timeseries` ultima
telemetria) → `eval_sensor_rules` → incidenti; PUSH webhook; regole sensori nostre (tenant admin);
AI tools tb_list_devices/tb_device_telemetry/tb_sync_devices. Mancano: alarm nativi TB, attributi,
RPC, shared-attr, storico, e operatività lato tenant admin.

## Gap → soluzione (ordine di chiusura)

### 🔴 Profondità ingest
1. **Alarm nativi TB non ingeriti.** `tb_pull_once` estende: GET `/api/v2/alarms?statusList=ACTIVE`
   → per ogni alarm applica **policy per severità** (CRITICAL/MAJOR/MINOR/WARNING/INDETERMINATE →
   ignora|registra|critico, default critico/critico/registra/registra/ignora) → `persist_ingest`
   (idem=`tb-alarm-{alarmId}` per dedup). critico→`sensore_critico` (escalation), registra→
   `allarme_sensore`, ignora→skip. Tipo+severità TB nel payload/dettaglio.
2. **Attributi non letti** (client/shared/server). Endpoint read `/values/attributes`.
3. **Storico telemetria** (solo latest oggi). Endpoint history `/values/timeseries?keys&startTs&endTs`.

### 🟠 Controllo device
4. **RPC / comandi al device** via TB. Endpoint POST `/api/plugins/rpc/{oneway|twoway}/{tbId}`
   {method, params}. operatore/admin.
5. **Shared attributes in scrittura** (config remota). Endpoint POST
   `/api/plugins/telemetry/DEVICE/{tbId}/attributes/SHARED_SCOPE`.

### 🟡 Gestibilità (requisito esplicito)
6. **Operatività lato tenant admin.** Oggi sync/connessione = solo superadmin. Aggiungo endpoint
   tenant_admin: inventario device TB (stato+telemetria), **sync manuale**, alarm policy, attributi,
   RPC, shared-attr, storico. La connessione resta al superadmin.
7. **Device profiles / alarm rules TB** gestibili da YW: NON in scope (le nostre `sensor_rules`
   sono la config; gli alarm TB-side ora comunque entrano via #1). Documentato come scelta.

## Modello dati
Nessuna tabella nuova. Alarm policy in `tenant_module_config` (thingsboard/alarm_map, chiave
`tb_alarm_severity`), letta/scritta direttamente (come Traccar event-policy). Dedup alarm via
idempotenza `persist_ingest` (idem = id alarm TB). Riuso `thingsboard_connections`,
`thingsboard_poll_state`, `devices`, `sensor_rules`.

## Endpoint
- superadmin (invariati): connessione, verify, sync.
- tenant_admin: `GET /api/thingsboard/devices` (inventario+stato+telemetria), `POST /api/thingsboard/sync`,
  `GET/POST /api/thingsboard/alarm-policy`, `GET /api/thingsboard/devices/:dev/attributes`,
  `POST /api/thingsboard/devices/:dev/shared-attributes`, `POST /api/thingsboard/devices/:dev/rpc`,
  `GET /api/thingsboard/devices/:dev/history`.
- `:dev` = nostro device uuid (source thingsboard) → risolve `tb_id` da `config`.

## UI (console)
- **Connettori → ThingsBoard**: salute + (tenant admin) **sync manuale**, **Policy allarmi**
  (severità→ignora/registra/critico), **inventario device** (stato/telemetria).
- Per device TB: **Attributi**, **RPC** (invia comando), **Config remota** (shared attr), **Storico**.

## Rollout / test
- `qa/thingsboard-parity-e2e.sh` contro TB vivo (:8084, tenant globex): alarm policy applica
  criticità, pull alarm→incidente, attributi, RPC (409 offline atteso), shared-attr, storico.
- Nessuna rottura: pull telemetria+sensor_rules invariato.

## Deliverable
`docs/thingsboard-manuale.md` — manuale admin (configurare + usare).

## Task
- [ ] #1 pull alarm TB + policy severità in tb_pull_once (idem dedup) + etype map
- [ ] alarm-policy get/set (tenant_module_config diretto) + default
- [ ] #6 endpoint tenant_admin: devices inventory + sync manuale
- [ ] #2 attributes read · #3 history · #4 rpc · #5 shared-attr write
- [ ] routes main.rs
- [ ] UI Connettori→ThingsBoard (policy + inventario + per-device attr/rpc/shared/history)
- [ ] QA e2e verde
- [ ] docs/thingsboard-manuale.md
