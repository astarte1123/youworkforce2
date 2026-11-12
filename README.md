# YouWorkforce — release candidate (DEMO)

Piattaforma modulare multitenant per forza lavoro sul campo, sicurezza lavoratori isolati,
IoT e consuntivi. Core **Rust** (Axum/SQLx), console **Go**, **PostgreSQL 18** con RLS.
**Motore di escalation nativo** (default, self-sufficient); GoAlert resta come rete di
sicurezza / rollback. L'escalation è condivisa da tutti i moduli che generano allarmi
(Sicurezza lavoratore, IoT, ThingsBoard, Traccar). Manuale: [`docs/escalation-manuale.md`](docs/escalation-manuale.md).

> **Verdetto onesto di questa build: DEMO.** La catena critica (SOS → incidente → GoAlert →
> presa in carico → chiusura) è riproducibile; isolamento multitenant applicato in database.
> Integrazioni esterne (GoAlert reale, badge Traccar, ThingsBoard, YouWorker.ai) sono
> **simulate e dichiarate**. Vedi [`qa/verdetti.md`](qa/verdetti.md). Non è PILOTA né PRODUZIONE.

## Cosa è implementato
- **Multitenancy + isolamento** end-to-end: RLS su Postgres 18, ruolo runtime `NOBYPASSRLS`, contesto tenant legato alla transazione. Tenant da host (browser) o da credenziale (macchina), mai dal payload.
- **Piano di controllo** (superadmin): crea tenant, nomina admin, abilita/disabilita moduli **senza deploy** (switch in UI); disattivazione bloccata se toglie un percorso essenziale.
- **Catena SOS**: ingress evento normalizzato idempotente → incidente (`aperto→preso_in_carico→intervento→chiuso`) → outbox transazionale → worker → GoAlert (SIMULATO) → timeline con correlazione. Anche SOS da PWA (sessione).
- **Check-in** con timer durevole nel core (orologio controllabile per i test), che scade senza nuovi dati badge → `mancato_checkin`. In PWA con countdown live.
- **Ordini di lavoro** (Prompt 9): bozza→assegnato→in_corso→completato→annullato, con concorrenza ottimistica (versione), RBAC e isolamento.
- **Consuntivi** (Prompt 9): bozza→in_revisione→da_correggere→approvato; righe ore + spese, calcolo deterministico in centesimi, doppia approvazione bloccata, **export CSV**.
- **Assistente AI** (Prompt 7, sola lettura): widget NL nella console con strumenti tipizzati eseguiti da Rust (check-in scaduti, sensori offline, storia incidente, ordini, moduli degradati). Tenant/permessi dal contesto autenticato; azioni critiche rifiutate; provider intercambiabile (ora pianificatore locale **SIMULATO**). Si abilita per tenant dal superadmin.
- **App lavoratore (Prompt 8)**: PWA con turni **confermati dal server** (inizio/fine idempotenti), **nota di lavoro dettata** (STT del browser via Web Speech, fallback manuale) con re-invio deduplicato e **coda offline** con indicatori distinti; SOS resta indipendente da voce/documenti. "Chiedi ai documenti" (RAG) = backlog dichiarato.
- **Configurazione strumenti per-modulo** (schema-driven, 2 livelli): ogni modulo espone *strumenti* configurabili con schema JSON (catalogo `module_tools`). **Superadmin** decide quali strumenti sono disponibili al tenant (`tenant_tool_enabled`); **tenant_admin** ne imposta i valori dalla console (`tenant_module_config`), validati lato server contro lo schema. Le configurazioni guidano il comportamento reale (es. loneworker/`checkin_timer` → default/clamp del check-in; thingsboard/`severity_gate` → soglia incidente). Isolamento RLS. Prova: `qa/module-config-e2e.sh` (10/10).
- **Gestione del tenant (Fase 1 — Lone Worker + Dispositivi)**: anagrafica **dispositivi** (CRUD, assegnazione sede/lavoratore, abilita/disabilita, config), **gruppi di allarme** + membri + **routing** evento→gruppo, **utenti** (invito/ruolo/password/elimina con salvaguardia ultimo admin), **sedi** (CRUD), **centrale** con assegnazione operatore + note e pannello **check-in attivi**. Tutto RLS per tenant, RBAC tenant_admin, audit. Prova: `qa/mgmt-e2e.sh` (20/20).
- **Connettori e IoT (Fase 4)**: gestione **credenziali di ingest** dal pannello (genera/copia/revoca token macchina — il tenant deriva sempre dal token), **riepilogo connettori** (Traccar/ThingsBoard/GoAlert/IoT: stato modulo, token, endpoint), **stato dispositivi** con ultimo contatto (telemetria/eventi). Prova: `qa/iot-mgmt-e2e.sh` (8/8).
- **Interventi pianificati (Fase 3)**: ordini di lavoro con **finestra temporale** e **priorità**, vista **Agenda** (pianificati) + "da pianificare", **modifica/ripianifica** con concorrenza ottimistica (versione). Orario e priorità mostrati anche nella PWA. Prova: `qa/interventi-e2e.sh` (8/8).
- **Forza lavoro e turni (Fase 2)**: **squadre** (team + membri), **turni pianificati** (pianificatore crea, lavoratore conferma, annullo/elimina) distinti dai turni reali timbrati, **reperibilità** (on-call) per utente. RLS per tenant, RBAC pianificatore/admin. Prova: `qa/workforce-e2e.sh` (14/14). Turni pianificati visibili e confermabili anche dalla PWA lavoratore.
- **UI**: design system completo (chiaro/scuro, responsive), console SPA (dashboard, centrale live con assegna/note/check-in, ordini, consuntivi, **turni**, **dispositivi**, **gruppi allarme**, **utenti e sedi/squadre**, moduli con **config strumenti**) con drawer e toast; PWA lavoratore mobile-first con turni pianificati.

## Non implementato (backlog, dichiarato)
Prompt 10 (parità/migrazione legacy). RAG "chiedi ai documenti" e STT lato server.
OpenDuty reale. SMS/voce (Twilio) e firmware badge hardware: da cablare per sito.

## Avvio — Docker Compose (profilo base)
```bash
docker compose up -d --build
# console:  http://localhost:8081
# core API: http://localhost:8080
```

## Avvio — nativo (sviluppo)
```bash
# 1) Postgres 18 (porta host 55432 per non collidere con un Postgres locale)
docker run -d --name ywdb -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=youworkforce -p 55432:5432 postgres:18

# 2) core (migrazioni + seed automatici)
cd core
export YW_DB_OWNER="postgres://postgres:postgres@localhost:55432/youworkforce"
export YW_DB_RT="postgres://app_rt:app_rt_pw@localhost:55432/youworkforce"
cargo run --release        # :8080

# 3) console
cd ../console && go run .   # :8081
```

## Test end-to-end (evidenza)
```bash
bash qa/smoke.sh            # 34 prove (T01..T06, T11, T13, T14) → 34 passed, 0 failed (GoAlert SIMULATO)
```

## GoAlert REALE (UPSTREAM REALE)
GoAlert v0.35.0 vero, con policy di escalation a 2 livelli. Il core crea allarmi reali.
```bash
bash qa/run-goalert.sh                      # avvia GoAlert + DB, migra, crea utenti (admin/oncall1/oncall2)
KEY=$(bash qa/goalert-setup.sh | sed -n 's/^KEY=//p')   # crea EP(2 livelli)+servizio+chiave generica
# avvia il core con GOALERT_BASE puntato a GoAlert:
#   export GOALERT_BASE=http://localhost:8082  (poi cargo run --release)
KEY=$KEY GA_PASS=goalertadmin1 bash qa/goalert-e2e.sh    # 6/6: allarme reale creato in GoAlert
```
Console GoAlert: http://localhost:8082 (admin / goalertadmin1). La chiave per-tenant si
imposta via superadmin: `POST /api/superadmin/tenants/{id}/goalert`.

**Consegna reale del 2° livello** (webhook, recapito di prova — nessun numero reale):
```bash
bash qa/goalert-delivery.sh   # 10/10: contact-method webhook verificato + allarme core consegnato davvero al receiver
```
Catena: core SOS → GoAlert → escalation → contact-method **verificato** → il receiver esterno
riceve la notifica con la correlazione del core. Trasporto **SMS/voce** (Twilio) = provider del cliente.

**GoAlert P1+P2+P3** (`qa/goalert-p123-e2e.sh`, 8/8, GoAlert reale):
- **P1 close-loop**: alla chiusura dell'incidente il core **chiude l'alert** GoAlert (`action=close`, stesso dedup); alert aperto con **details** (sede/lavoratore/lat-lon/correlazione) e **AlertID** salvato sull'incidente. Timeline `goalert_sent` (inviato) vs `goalert_closed`.
- **P2 bidirezionale**: `GET /api/incidents/:id/goalert-status` legge lo **stato reale** via GraphQL (non preso in carico / ack / chiuso); `GET /api/oncall-now` mostra **chi è reperibile** ora (badge in centrale).
- **P3 provisioning/robustezza**: `POST /api/superadmin/tenants/:id/goalert/provision` crea **escalation policy + servizio + integration key + heartbeat monitor** via GraphQL (bottone in superadmin); il worker fa **ping heartbeat** (GoAlert allerta se la pipeline muore); **routing per tipo evento** → key/service diversi (`/api/goalert/routing`). Credenziali GraphQL per-tenant (vault in produzione).

## Traccar REALE — sorgente badge (UPSTREAM REALE)
Traccar v6.17.0 vero. Il device parla il protocollo OsmAnd; Traccar genera l'evento
di allarme e lo inoltra (event forwarding) all'adapter del core, che lo mappa sulla
stessa catena SOS. Due upstream reali in catena: Traccar → core → GoAlert.
```bash
bash qa/run-traccar.sh        # avvia Traccar, registra il device BADGE-001, imposta il forwarding
bash qa/traccar-e2e.sh        # 8/8: OsmAnd SOS -> incidente -> GoAlert (UPSTREAM REALE)
# invio manuale di un SOS da "device":
curl "http://localhost:5055/?id=BADGE-001&lat=45.47&lon=9.19&timestamp=$(date +%s)&alarm=sos"
```
Console Traccar: http://localhost:8083. **Mappa allarmi orientata al lavoratore isolato** (P1):
`sos→sos` (configurabile man_down), `fallDown/accident→man_down`, `removing/tampering/bonnet→manomissione_badge`,
`powerCut/powerOff→perdita_alimentazione`, `jamming/gpsAntennaCut→jamming` (tutti **critici**);
`lowBattery/lowPower→batteria_bassa` (warning); `deviceOffline→perdita_contatto` se abilitato in `traccar/alarm_map`
(`offline_critical`); `batteryLevel` sotto `battery_min` → `batteria_bassa`. Prova: `qa/traccar-p1-e2e.sh` (13/13).
**P2 — posizioni & presenza**: ingest del *position forwarding* (`/api/ingress/traccar/position`, {position,device})
→ ultima posizione nota su ogni device (lat/lon/batteria/ora) + storico (`/api/devices/:id/positions`),
idempotente, no-regressione su fix più vecchi; pannello device con **Apri mappa** (OSM) e **Percorso recente**;
`geofenceEnter/Exit` → presenza (`ingresso_sito`/`uscita_sito`). Il `run-traccar.sh` imposta sia event che position forwarding.
Prova: `qa/traccar-p2-e2e.sh` (12/12).
**P3 — mappa & immobilità**: vista **Mappa** (Leaflet + OpenStreetMap) con i device posizionati, popup (etichetta/batteria/ora/assegnatario), **percorso recente** (polyline) e **aggiornamento live** via SSE `device_position`; opzione `stopped_as_immobilita` (`deviceStopped`→`immobilita`, incidente). Prova: deviceStopped in `qa/traccar-p1-e2e.sh` (15/15).
Il tenant deriva dal token di ingest (`?token=` o header `X-Ingest-Token`), **mai dal body**.

## ThingsBoard REALE — sorgente IoT (UPSTREAM REALE)
ThingsBoard CE v3.8.1 vero. La telemetria del device attraversa il **Rule Engine** reale;
un nodo *REST API Call* (aggiunto alla Root Rule Chain) inoltra al core, che applica il
gate di severità: oltre soglia → `sensore_critico` → incidente → GoAlert.
```bash
bash qa/run-thingsboard.sh    # avvia TB, cabla il nodo REST nella rule chain, crea device SENSOR-DEF (richiede python)
bash qa/thingsboard-e2e.sh    # 8/8: telemetria critica -> incidente -> GoAlert; non-critica -> nessun incidente
```
Console ThingsBoard: http://localhost:8084 (tenant@thingsboard.org / tenant).
ThingsBoard CE è **BUSL**: eseguirlo come istanza esterna/standalone.

## Credenziali demo (recapiti di prova)
| Ruolo | Dominio/slug | Utente | Password |
|---|---|---|---|
| Superadmin | tenants (control plane) | `admin@loneworker.eu` | `admin` |
| Tenant admin | acme | `admin@acme.loneworker.eu` | `demo` |
| Operatore centrale | acme | `operatore@acme.loneworker.eu` | `demo` |
| Lavoratore (PWA) | acme | `mario@acme.loneworker.eu` | `demo` |
| Auditor (sola lettura) | acme | `auditor@acme.loneworker.eu` | `demo` |
| Tenant secondario (isolamento) | globex | `admin@globex.loneworker.eu` | `demo` |

Token ingest macchina: `acme-ingest-token`, `globex-ingest-token`.

> In locale usa lo slug tenant nel campo apposito delle pagine (header `X-Tenant-Slug`).
> In produzione il tenant deriva dal dominio `{slug}.loneworker.eu`.

## Struttura
```
contracts/   glossario, OpenAPI, schema evento, matrice moduli, regole isolamento
core/        Rust: API + worker + migrazioni (RLS)
console/     Go: proxy + UI superadmin/centrale + PWA lavoratore
qa/          smoke test, matrice accettazione, runbook, verdetti, offerta pilota
```

## Sicurezza / limiti noti (demo-grade, da irrobustire per pilota)
- Password: **argon2id** (PHC) con verifica retro-compatibile e re-hash al login (`qa/auth-hardening.sh`).
- Sessioni: **persistenti su DB** (sopravvivono al riavvio), con scadenza 12h e **logout server-side** (`qa/session-hardening.sh`). Cookie `HttpOnly; SameSite=Lax`.
- **CSRF**: protezione double-submit sulle mutazioni cookie-autenticate (header `X-CSRF-Token` deve combaciare col cookie `yw_csrf`); ingress macchina e login/logout esenti (`qa/csrf-hardening.sh`). TLS/MFA restano per il pilota.
- **Rate-limit login**: 5 tentativi errati / 5 min per chiave (slug·email·IP) → `429 Retry-After` (`qa/ratelimit-hardening.sh`). Store **condiviso su DB** (`app.login_throttle`) → valido anche multi-istanza.
- **Scalabilità multi-istanza**: core **stateless** (sessioni e throttle su DB), isolamento RLS, worker outbox con `FOR UPDATE SKIP LOCKED` (sicuro in concorrenza), eventi live SSE via **Postgres LISTEN/NOTIFY** → più istanze core condividono gli eventi (`qa/sse-scale.sh`). Resta da scalare il solo Postgres (replica/partizione per tenant).
- Nessun TLS/MFA/CSRF configurati nel profilo locale; richiesti per il pilota.
- `TEST_CLOCK=1` espone `/api/_test/advance`: **disabilitare** (`TEST_CLOCK=0`) fuori dai test.
- GoAlert SIMULATO: un ack locale non prova la consegna reale.
