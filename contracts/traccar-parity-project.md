# Progetto: Traccar Parity — chiusura gap YouWorkforce ↔ Traccar

Stato: COMPLETO (e2e 17/17 + effetto policy verificato su Traccar vivo) · Owner: core/api + console · Motore: Traccar container (:8083 dev)
Principio guida: **tutto configurabile da YouWorkforce, in modo semplice.** L'admin non apre
mai la console Traccar. Default sicuri (lone-worker first), un click per applicare.

## Contesto
Audit doc Traccar (traccar.org/documentation) vs codice (`map_traccar_etype`, `ingest_traccar`,
`traccar_poll`, `device_command`, `traccar_connections`). Oggi YouWorkforce usa: connessione +
server settings, ingest position/event forwarding, pull `/api/positions` + `/api/reports/events`
con watchdog, device mirror + batteria, mappa alarm→incidenti (SOS/man_down/tampering/powerCut/
jamming/lowBattery, offline, stopped→immobilità, geofenceEnter/Exit), presence nativa
(point-in-polygon), comandi generici. Mancano 14 aree.

## Gap → soluzione (ordine di chiusura)

### 🔴 Eventi safety (basso costo, alto valore lone-worker)
1. **Unaccompanied Motion + Proximity Enter/Exit** — mai mappati (→ generico).
2. **Device Inactive** — "lavoratore fermo/scomparso", diverso da offline.
3. **Eventi non specializzati**: Overspeed, Ignition On/Off, Fuel Drop/Increase, Maintenance,
   Driver Changed, Media, Device Moving, geofenceCrossed → oggi tutti `traccar_event`.
→ **Event policy configurabile** per-tenant: ogni tipo evento Traccar → handling
   `ignora | registra | critico` con default sicuri (unaccompaniedMotion/deviceInactive=critico).
   `map_traccar_etype` consulta `alarm_map.events`; criticità via convenzione etype `*_critico`
   (una riga in `persist_ingest`). Tipi Traccar reali presi da `/api/notifications/types`.

### 🟠 Tracciamento / field workforce (proxy dei report Traccar, già pronti)
4. **Trips & Stops** — `/api/reports/trips` + `/api/reports/stops` per device+periodo.
5. **Report**: route/summary oltre a trips/stops; export CSV. Scheduling = resta al sistema
   report YW (non si duplica).
6. **Drivers** (#6) — CRUD proxy su `/api/drivers` + link device (`/api/permissions`); evento
   Driver Changed mappato (via #3); `driverUniqueId` mostrato su device/posizione.
7. **Maintenance** (#7) — CRUD proxy su `/api/maintenance` (totalDistance/hours/fixTime, start,
   period) + link device; evento `maintenance` mappato (via #3).
8. **Computed Attributes** (#8) — CRUD proxy su `/api/attributes/computed` (description,
   attribute, expression JEXL, type, priority) + link device. Flessibilità espressioni via Traccar.

### 🟡 Comfort
9. **Route replay** — disegno percorso del device per periodo (da `/api/reports/route`).
10. **Accumulators** — lettura odometro/ore + reset via `PUT /api/devices/{id}/accumulators`.
11. **Media** — evento `media` mappato (via #3), link al file conservato negli attributi.
12. **Catalogo comandi** — dropdown dei comandi comuni (richiesta posizione, engineStop/Resume,
    positionPeriodic, custom) nella UI comandi (oggi solo type libero).

### ⚪ Non-gap (coperti diversamente, documentato)
- **Notificators** Traccar (email/SMS/Telegram/WhatsApp/push): instradiamo via GoAlert.
- **Calendars** su notifiche/geofence: lo scheduling è nostro (vedi copertura in [[goalert-parity]]).
- **Geofence push a Traccar**: presence calcolata nativamente (no doppi eventi). Opzionale futuro.
- **Forwarding** json/kafka/AMQP/MQTT: usiamo HTTP forward.
- **Auth** OpenID/LDAP/TOTP, horizontal scaling, scelta DB, filtri/geocoding server: motore/infra.

## Modello dati (migrazione 0051)
Niente mirror locale per drivers/maintenance/computed: **single source of truth = Traccar**
(proxy live). Solo:
- `alarm_map.events` (dentro tool_config `traccar/alarm_map`, nessuna tabella nuova) per l'event policy.
- opzionale `traccar_media(tenant_id,device_id,evt_id,url,kind,at)` se si conserva il riferimento media.
(Se basta leggere live, 0051 può essere vuota o solo il default `events`.)

## Endpoint (tenant_admin, modulo traccar; comandi operatore+admin)
- `GET/POST /api/traccar/event-policy` — policy eventi (merge in alarm_map.events).
- `GET /api/traccar/reports/{trips|stops|summary|route}?device_id&from&to` (+ `.csv`).
- `GET/POST/DELETE /api/traccar/drivers` (+ link device).
- `GET/POST/DELETE /api/traccar/maintenance` (+ link device).
- `GET/POST/DELETE /api/traccar/computed` (+ link device).
- `GET/POST /api/traccar/accumulators/{device_id}` (lettura/reset).
- comandi: catalogo statico esposto in UI (nessun nuovo endpoint; riuso `device_command`).

## UI (console)
- **Dispositivi**: per device → Report (trips/stops/summary/route+replay), Accumulators,
  comandi con catalogo, driver corrente.
- **Connettori → Traccar**: **Policy eventi** (griglia tipo→ignora/registra/critico),
  **Drivers**, **Manutenzioni**, **Attributi calcolati** (CRUD semplici con link device).

## Rollout / test
- `qa/traccar-parity-e2e.sh` contro Traccar vivo (:8083, tenant globex): event policy applica
  criticità, reports trips/stops/summary, drivers/maintenance/computed create+link, accumulators.
- Nessuna rottura: eventi già gestiti invariati; default policy non altera il comportamento SOS.

## Deliverable finale
`docs/traccar-manuale.md` — manuale admin: configurare + usare, funzione per funzione.

## Task — tutte COMPLETE
- [x] #1-3 event policy in map_traccar_etype + suffisso `_critico` in persist_ingest + skip `ignore`
- [x] endpoint GET/POST event-policy + default (verificato: overspeed=critico → incidente; moving=ignora → scartato)
- [x] #4-5 proxy reports trips/stops/summary/route (+CSV)
- [x] #6 drivers CRUD+link
- [x] #7 maintenance CRUD+link
- [x] #8 computed attributes CRUD+link
- [x] #10 accumulators get/reset
- [x] #12 catalogo comandi UI (devCmdTypes già presente, esteso) · #11 media via policy
- [x] UI Connettori→Traccar (🛠 avanzato) + Dispositivi (Report + Contatori)
- [x] QA e2e verde — `qa/traccar-parity-e2e.sh` 17/17 su Traccar vivo
- [x] docs/traccar-manuale.md

## Verifica (e2e 17/17, Traccar :8083, tenant globex)
event policy get/set · reports trips/stops/summary · drivers/maintenance/computed create+link+del ·
accumulators get/reset. Effetto end-to-end: ingress deviceOverspeed(policy critico)→incidente
`eccesso_velocita_critico` aperto; deviceMoving(policy ignora)→scartato.
Regressione: traccar-e2e 8/8, traccar-config 4/4, battery-policy 7/7, smoke 34/34.
Note: #9 route replay non implementato come animazione (c'è il trail live + report route/CSV);
geofence-push e calendars restano N/A (presence/scheduling nativi).
