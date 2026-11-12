# YouWorkforce — Razionalizzazione perimetro "Worker Safety"

> Scritto per: team prodotto + backend (Rust) + front-end. Analisi su codice reale (censimento
> 2026-11-11). Complementare, non sostitutivo, di `ui-rationalization.md` (IA generale della console)
> e `native-escalation-engine-project.md` (motore escalation). Qui si definisce **il perimetro unico
> della sicurezza del lavoratore** e il flusso lineare device→worker→allarme→escalation→gestione.
> Scope separato dal mondo IoT/Scenari ambientali (decisione: perimetri distinti).

## 0. Requisito (parole dell'utente)

Ogni funzione legata al **man-down e alla sicurezza dei lavoratori** deve avere un **perimetro preciso**
in YW che permetta di:
1. configurare **1..n dispositivi** e **1..n app** che i lavoratori indossano;
2. far **autenticare** quei dispositivi/app sulla piattaforma;
3. in runtime, se al lavoratore capita qualcosa → **inviare un allarme**;
4. gestirlo con **modalità di escalation configurabili**.

## 1. Modello-perimetro (catena unica, 5 stadi)

```
[1] Dispositivi & App        → identità + autenticazione su YW
        │   (indossati dal lavoratore)
        ▼
[2] Lavoratore               → identità + ASSOCIAZIONE worker ↔ device(i)/app
        │
        ▼ (lavoro in campo)
[3] Evento → Allarme         → man-down / SOS / immobilità / check-in mancato / fuori-area
        │                       → incidente critico (single-path, mai perso)
        ▼
[4] Escalation               → chi/ordine/canale/ripetizione/timeout (configurabile)
        │
        ▼
[5] Gestione                 → ack / close / audit in Centrale
```

L'entità che oggi **manca** è il legante dei 5 stadi: un **"Profilo di protezione"** per lavoratore
(device+app+trigger abilitati+policy escalation). Vedi §4-C.

## 2. Stato attuale (sintesi del censimento) — cosa c'è / gap

| Stadio | C'è (file:linea) | Gap |
|---|---|---|
| 1 Device auth | `ingest_tokens` per-tenant+source `0002_ingest.sql`; ingress `api.rs:3500/3673/4567/4779`; catalogo modelli cap `sos,fall` `0024` | **no credenziale per-device**, token in chiaro, no scadenza/rotazione; token Traccar anche in querystring (`api.rs:4574`) |
| 1 App | `worker_sos api.rs:4909`; login cookie HttpOnly+CSRF+argon2 `api.rs:2567`; PWA `worker/index.html` | **no geolocalizzazione** nella PWA; nessun bearer/JWT per app mobile vera |
| 2 Worker↔device | `devices.assigned_user` `0001_init:74`; `assign_device api.rs:2937`; UI `app.html:1344` | associazione **solo informativa** (report/JOIN), non vincola auth né correla il fix al lavoratore/token |
| 3 Trigger | `map_traccar_alarm api.rs:21` (sos/fallDown→man_down/deviceStopped→immobilità); `sweep_checkins worker.rs:501`; `conn_watchdog worker.rs:109`; `persist_ingest api.rs:3590`; `CRITICAL api.rs:14` | no_motion/dead-man letterali assenti (coperti da `immobilita`+`mancato_checkin`); **fuori-area non critico di default** |
| 4 Escalation | nativo `escalation.rs`+`delivery.rs`; flag `escalation_engine` `api.rs:6522/6545` mig `0056`; GoAlert upstream | **default=goalert**, nativo in shadow → doppio path; UI config sotto sezione "GoAlert"; Slack per-gruppo non cablato; UTC-only |
| 5 Gestione | `ack_incident/close_incident api.rs:4862/4883` (operatore/admin/reperibile); `vCentrale`/`vMieiAllarmi` | naming legacy "GoAlert" nella config |

**Diagnosi:** il flusso esiste tutto ma è **frammentato** su 3 mondi (Traccar/ThingsBoard/loneworker)
+ 2 motori escalation + UI sparsa. I difetti strutturali sono: anello 1↔2 non vincolante, app lavoratore
senza geoloc, doppio motore escalation, nessun oggetto unificante.

## 3. Priorità scelta: **Fase D+E** (UI unica + escalation nativa default)

> A/B/C (identità per-device, app geoloc, entità Profilo di protezione) restano pianificate ma **dopo**.
> D+E riordinano la superficie e tolgono il doppio motore: basso rischio, alto guadagno di chiarezza,
> abilitano le fasi successive.

### E — Menu unico "Sicurezza Lavoratore"

Oggi gli allarmi/sicurezza vivono su più voci (Centrale, Mappa, Piantine, Report, "Gruppi allarme",
sezione GoAlert). Si introduce **un gruppo nav dedicato** che raccoglie l'intero perimetro, in ordine
di flusso di configurazione poi operatività:

| Gruppo nav | Voci (nuova etichetta ← attuale) | Chi vede |
|---|---|---|
| **Sicurezza Lavoratore** → *Operatività* | Centrale allarmi · I miei allarmi · Mappa (assorbe Piantine) | operatore, admin, auditor, reperibile, responsabile |
| **Sicurezza Lavoratore** → *Configurazione* (admin) | Dispositivi & App · Lavoratori e assegnazioni ← Anagrafiche(sottoinsieme) · Trigger di sicurezza · Escalation e reperibilità ← Gruppi allarme/GoAlert | tenant_admin |

- Riusa il meccanismo `NAV` con `group` già previsto da `ui-rationalization.md §1`.
- Allineato al glossario (`ui-rationalization.md §10.6`): **Reperibilità** (non on-call), **Escalation**,
  **Presa in carico** (non Ack), **Area** (non geofence), **Planimetria** (non piantina).
- "Trigger di sicurezza" = pagina che mostra/abilita per-tenant quali etype critici sono attivi
  (sos, man_down, immobilita, mancato_checkin, fuori-area) e la loro mappatura sorgente (Traccar
  alarm → etype, config `sos_as`/`stopped_as_immobilita`), oggi sparsa nel codice (`map_traccar_alarm`).

### D — Escalation nativa come default + de-branding GoAlert

1. **Flip del default** `tenant_oncall_cfg.escalation_engine` da `goalert` → `native` (nuova migrazione;
   GoAlert resta selezionabile per rollback per-tenant via `escalation_engine_set api.rs:6545`).
2. **Soppressione doppio-path** attiva quando `native` (`esc_suppress_goalert api.rs:6533`), così un
   incidente non notifica due volte.
3. **UI**: rinominare la sezione "GoAlert" → **"Escalation e reperibilità"**; i canali SMS/voce/mail/Slack
   diventano "Canali di notifica"; il motore (native/goalert) è un dettaglio avanzato/superadmin, non il
   titolo della funzione.
4. **Contratto da aggiornare**: `module-matrix.md` riga 22 ("GoAlert è l'unico motore… Nessun secondo
   motore") è **obsoleta** → sostituire con "motore escalation = nativo di default, GoAlert opzionale per
   rollback". Allineare `native-escalation-engine-project.md` (Fase 3 cutover → promuovere a default).

### Residui da chiudere in D (già tracciati)

- Slack per-gruppo nella ladder per-incidente (adapter pronto `delivery.rs`, cablaggio mancante).
- DST/fuso: conversione a monte del resolver UTC-only (`escalation.rs`).
- Email live senza relay resta dry/sink (documentare il requisito `YW_ESC_SMTP_RELAY_URL`).

## 4. Fasi successive

- **A — Identità per-device. ✅ FATTA** (mig 0058). `ingest_tokens` esteso: `device_id` (bind opzionale a un
  dispositivo), `token_hash` (sha256 a riposo per i nuovi token; i legacy restano in chiaro finché ruotati),
  `label`, `expires_at`, `revoked_at`, `last_used_at`. Lookup centralizzato in `resolve_ingest_token()`
  (match per hash o plaintext legacy; rifiuta revocati/scaduti; aggiorna last_used). I 4 ingress
  (events/traccar/traccar-position/thingsboard) usano il resolver e, se il token è device-bound,
  **forzano l'external_id del device legato** (`bound_device_external`) → anti-spoof: un payload non può
  spacciarsi per un altro device. API: create con device_id/label/expires_days (segreto mostrato UNA volta),
  `POST /api/ingest-tokens/:handle/revoke`. UI: card "Credenziali di ingest" con bind device, scadenza,
  stato (attivo/scaduto/revocato), ultimo uso, badge hash, banner segreto one-time. Verifica live globex:
  token bound→DHT11, ingest con device spoofato→attribuito a DHT11, revoca→401. RESIDUO: token in
  querystring ancora accettato (compat Traccar) — mitigare leak nei log a parte.
- **B — App lavoratore reale. ✅ FATTA.** PWA (`console/static/worker/index.html`): SOS con fix GPS
  reale (`enableHighAccuracy`, accuratezza inviata) + **tracking continuo** `watchPosition` (throttle ~15s)
  verso `POST /api/worker/position`, con card "Condivisione posizione" (toggle), auto-on all'inizio turno /
  auto-off a fine turno, ripristino da localStorage. Backend: `worker_position` (session-auth, modulo
  loneworker) aggiorna ultima posizione + storico + presenza geofence; `worker_app_device()` risolve il
  **device del lavoratore** (devices.assigned_user) o **auto-provisiona** un device `source='pwa'`
  external_id `pwa-<uid>` intestato all'utente → **bind sessione↔device**: il fix è SEMPRE attribuito al
  device dell'utente autenticato, mai a un id dal payload. `worker_sos` ora scrive `device_id` + `accuracy_m`.
  Verifica live globex: position→device pwa auto-creato e aggiornato; SOS→attribuito al device + accuratezza.
- **C — Entità "Profilo di protezione". ✅ FATTA** (mig 0059). Record che lega `1..n lavoratori + trigger
  abilitati + policy escalation + check-in` → unico punto di config del perimetro; i **device coperti**
  derivano da `devices.assigned_user` dei membri. Tabelle `app.protection_profiles`
  (name, descr, `escalation_group_id`→alarm_groups, `triggers` jsonb, `checkin_minutes`, `enabled`) e
  `app.protection_profile_members` (UNIQUE `(tenant_id,user_id)` → un worker in **al massimo un** profilo;
  l'add lo **sposta** dal precedente). API tenant_admin/loneworker: `GET/POST /api/safety/profiles`,
  `PUT/DELETE /api/safety/profiles/:id`, `POST /api/safety/profiles/:id/members`,
  `DELETE /api/safety/profiles/:id/members/:uid`. UI app.html: voce nav "Profili di protezione" (gruppo
  safety) + `vProfili` (editor per-profilo: nome/descr/enabled, select gruppo escalation, trigger
  checkbox, check-in, membri add/remove, device coperti).
  **Aggancio runtime (il valore del profilo):** nel runner escalation nativo
  (`escalation_delivery_shadow_tenant`), per ogni incidente aperto il gruppo è
  `COALESCE(profilo.escalation_group_id, alarm_routing.group_id)` risolto via
  `incident → (assigned_user | event.device.assigned_user) → profile_member → profile(enabled)`. Quindi
  un profilo attivo con gruppo **ha la precedenza** sul routing globale per-etype per i suoi lavoratori;
  profilo disattivo o senza gruppo → **fallback** al routing globale (comportamento legacy invariato).
  Verifica live globex: 13/13 CRUD (create/update/delete, 409 dup, move 1-worker-1-profilo, readback
  con group_name/triggers/device coperti) + 3/3 override routing (attivo→gruppo profilo,
  disattivo→globale, senza-gruppo→globale); loneworker-e2e 15/15 (nessuna regressione).
  **Gating trigger per-profilo. ✅ FATTO.** Helper `trigger_suppressed(st,tid,worker,device_id,etype)`
  (api.rs, pub(crate)) con mappa etype→chiave (sos/man_down/immobilita/mancato_checkin/attraversamento_geofence→geofence).
  Principio **FAIL-OPEN**: sopprime SOLO se esiste un profilo **attivo** per quel worker (risolto via
  `worker` esplicito o `device.assigned_user`) e la chiave trigger è **esplicitamente `false`**; nessun
  profilo / etype non mappato / chiave assente o `true` → non sopprime (il single-path critico non si
  perde per errore). Cablato nei 3 siti di apertura incidente: `persist_ingest` (sos/man_down/immobilita/
  geofence da macchine), `worker_sos` (SOS app), `sweep_checkins` (mancato_checkin, worker.rs). Quando
  sopprime: l'evento resta a trail, l'incidente NON si apre, audit `trigger_suppressed_by_profile`.
  Verifica live globex: app-SOS sos=false→soppresso+audit / sos=true→incidente / profilo disattivo→
  fail-open; persist_ingest (device traccar assegnato) sos=false→soppresso, man_down→passa (selettivo);
  loneworker-e2e acme (senza profili) 15/15 (fail-open confermato). 7/7 gating e2e.

## 5. Piano operativo D+E (step verificabili)

1. **Nav**: aggiungere gruppo `safety` in `NAV` (`app.html`), spostare Centrale/I miei allarmi/Mappa +
   le voci di config sotto le due sezioni; icone distinte (`ui-rationalization.md §10.2`).
2. **Pagina "Trigger di sicurezza"** (lettura+toggle etype critici per-tenant; sorgente→etype).
3. **Rinomine UI** sezione GoAlert → Escalation e reperibilità; "Canali di notifica".
4. **Migrazione** `00XX_escalation_engine_default_native.sql`: default colonna → `native` (nuovi tenant);
   script opt-in per flippare gli esistenti; GoAlert resta per rollback.
5. **Soppressione path** verificata: un incidente native → una sola catena di consegna (no outbox GoAlert).
6. **Aggiornare contratti**: `module-matrix.md`, `native-escalation-engine-project.md`.
7. **Verifica live** su tenant pilota (globex): incidente SOS → una sola escalation nativa, ack/close dalla
   Centrale, audit coerente.

## 6. Criteri di "fatto" (D+E)

- Un solo menu copre l'intero perimetro sicurezza; nessuna voce sicurezza fuori dal gruppo.
- Nessun incidente critico genera doppia notifica (native default, GoAlert soppresso salvo rollback).
- La parola "GoAlert" non compare più nella UI tenant (resta nei dettagli superadmin/avanzati).
- I contratti non si contraddicono sul numero di motori escalation.

## 7. F1+F2 — Escalation disaccoppiata e self-sufficient (FATTO 2026-11-12)

Analisi gap vs GoAlert (goalert.me/docs) → scelta F1+F2. Migrazione `0061_native_escalation_selfsufficient.sql`.

**F1 — Disaccoppiamento modulo + maintenance nativo**
- L'escalation/centrale è condivisa da OGNI modulo che produce incidenti: const `ESCALATION_MODS =
  [loneworker, traccar, iot, thingsboard]`. Gli endpoint di configurazione/visibilità (gruppi, routing,
  incidenti, on-call settings, recapiti, override, maintenance) passano da `require_module("loneworker")`
  a `require_any_module(ESCALATION_MODS)`. Così con "Sicurezza lavoratore" spento ma IoT/TB/Traccar attivo
  l'admin vede e configura l'escalation che instrada i suoi allarmi. `trigger`/`profili` restano loneworker.
- Sanata l'asimmetria: le mutation gruppi/routing (prima senza gate) ora sono gated come i GET.
- Nav app.html: `centrale`, `miei-allarmi`, `gruppi` visibili ai 4 moduli.
- Maintenance NATIVO: `tenant_oncall_cfg.maintenance_until`; il runner nativo sospende le consegne nella
  finestra (prima era solo su GoAlert = no-op per tenant nativi). GoAlert allineato best-effort se presente.

**F2 — Autosufficienza del motore nativo (no GoAlert richiesto)**
- Il runner `escalation_delivery_shadow_tenant` non richiede più una sessione GoAlert; GoAlert resta
  ground-truth opzionale solo per la semina rotation (compat shadow).
- Override/assenze NATIVI: tabella `esc_overrides` (CRUD nativo; mirror GoAlert best-effort per rollback).
- Seed rotation NATIVO (posizione 0 deterministica) quando non c'è GoAlert; in shadow resta `None` per
  non generare false divergenze (param `allow_native_seed`).
- Verifica recapiti NATIVA: OTP via SMS (`users.phone_verified_at/phone_verify_code/_expires`), adapter
  Twilio live o sink/dry. Endpoint recapiti riscritti native (richiesta `{user_id}`).
- Scan consegna = tenant con incidenti aperti UNION goalert_services (non più solo goalert).

**Verifica:** `qa/escalation-decouple-e2e.sh` 13/13 (globex loneworker OFF + thingsboard ON → escalation 200;
override + OTP + consegna nativi senza GoAlert; maintenance sospende/riprende) + `loneworker-e2e.sh` 15/15.

**Nota operativa (RLS):** `YW_DB_RT` deve connettersi come `app_rt` (NOBYPASSRLS), non come `postgres`
(superuser, bypassa RLS). L'isolamento tenant dipende dal ruolo non-superuser + `set_config(app.current_tenant)`.

**Gap residui (F3, non fatti):** builder policy multi-step con target misti per-step; temporary schedule;
monthly rotation (UI/DB ok, resolver no); multi-recapito + regole notifica per-utente; dedup esplicito +
bundling; label/ricerca; annunci Slack schedulati; iCal dalla rotazione calcolata; UI per report/engine-toggle.

## 8. F3-core — Parità policy escalation (FATTO 2026-11-12)

Migrazione `0062_escalation_policy_steps.sql`.

**Policy escalation multi-step per gruppo** (parità GoAlert: step ordinati con canale+attesa, reorder,
repeat-up-to). Tabella `esc_policy_steps` + `alarm_groups.esc_repeat`. Se un gruppo ha step custom il
runner li usa (policy costruita PER-GRUPPO dentro il loop di consegna), altrimenti ladder derivata
(SMS→VOICE) retro-compatibile. CRUD `GET/POST /api/groups/:id/policy` (replace-all). UI: editor "⛓ Policy
escalation" nella scheda gruppo (add/rimuovi/riordina step, canale, attesa minuti, repeat).

**Monthly rotation** calendar-aware (`escalation.rs`: `RotationType::Monthly`, `add_months`, `monthly_k`;
clamp all'ultimo giorno del mese, boundary reali). Prima era approssimata a Weekly×4.

**UI diagnostica motore** in Escalation: report consegne (`/escalation/delivery`) e parità shadow
(`/escalation/shadow`) + badge motore corrente.

**Verifica:** `qa/escalation-policy-e2e.sh` 8/8 + `escalation::tests` 12/12 + non-regress decouple 13/13 +
loneworker 15/15.

**Residui F3 (prossimi incrementi):** temporary schedule; multi-recapito + regole notifica per-utente
(ladder a 2 livelli policy×utente); dedup esplicito + bundling; label/ricerca; annunci Slack schedulati;
iCal dalla rotazione calcolata; engine-toggle in UI superadmin.

## 9. F3-residui — Parità GoAlert completata (FATTO 2026-11-12)

Migrazione `0063_escalation_parity_rest.sql`. Chiusi tutti i gap residui:

- **Temporary schedule** (`esc_temp_schedules`): sostituzione a tempo; mentre attiva solo l'utente del
  blocco corrente è on-call. `GET/POST /api/oncall-temp-schedules`, `/delete`. UI nella sezione avanzata.
- **Multi-recapito + regole di notifica per-utente** (`user_contact_methods`, `user_notif_rules`): ladder a
  2 livelli policy×utente. Il runner espande ogni on-call nelle SUE regole (contact method + ritardo),
  altrimenti fallback al canale dello step. `/api/oncall-contacts/:uid/methods|rules` (CRUD). UI nei recapiti.
- **Dedup esplicito** (parità dedup key GoAlert): un 2° critico con la stessa `correlation` di un incidente
  aperto non ne apre un altro (`deduplicated:true`, timeline `dedup_suppressed`).
- **Bundling maintenance**: gli incidenti accumulati durante la finestra generano UN riepilogo a fine finestra.
- **Label/ricerca** (`alarm_groups.labels` jsonb): `POST /api/groups/:id/labels`, `GET /api/groups/search`.
- **Annunci Slack schedulati** (`slack_announce_scan` nel worker): onchange (handoff) o orario giornaliero.
- **iCal dalla rotazione calcolata**: il feed include i turni calcolati dal resolver nativo (30 giorni).
- **Engine-toggle in UI superadmin**: select nativo/GoAlert per tenant.

**Verifica:** `qa/escalation-rest-e2e.sh` 10/10; regression policy 8/8, decouple 13/13, loneworker 15/15,
unit 12/12. Slack live richiede token (non testabile in dev). La verifica OTP su contact method aggiuntivi
(oltre al telefono primario) resta una nicety non implementata.

## 10. Parità pagina Profile GoAlert — G1-G4 (FATTO 2026-11-12)

Migrazione `0064_profile_parity.sql`. Tutto gated `ESCALATION_MODS` (config tenant admin, modulo superadmin).

- **G1 — Opt-out / Do-Not-Call (compliance):** registro globale `contact_optout` (per numero/email,
  persiste oltre lo stato del modulo). SMS **STOP** / voce **tasto 9** disabilitano il recapito; il
  runner **salta** i recapiti in opt-out. **START** (SMS) o **Riabilita** (admin) tolgono dal DNC e
  richiedono ri-verifica. Fix: `esc_is_opted_out` ora usa `EXISTS(...)::bool`.
- **G2 — Verifica OTP sui recapiti aggiuntivi:** ogni contact method si verifica con codice
  (`/api/oncall-contacts/methods/:id/verify|code|test`).
- **G3 — Alert Status Updates per-utente + consegna nativa:** `users.status_updates_on` (override del
  default tenant); alla presa in carico/chiusura gli ALTRI on-call con opt-in ricevono un avviso
  nativo (rispetta opt-out). Prima il flag era solo GoAlert.
- **G4 — Note di passaggio turno + playbook:** `esc_handoff_notes` per gruppo; `alarm_groups.playbook`
  ("cosa fare") mostrato nella config gruppo.

**Divergenza voluta da GoAlert:** YouWorkforce **non blocca** la consegna su recapito non verificato
(priorità: non perdere mai un SOS); il blocco duro è solo l'**opt-out** (compliance).

**Verifica:** `qa/escalation-profile-e2e.sh` 14/14. Regression totale: profile 14 + rest 10 + policy 8 +
decouple 13 + loneworker 15 + escalation unit 12. Manuale `docs/escalation-manuale.md` aggiornato.

## 11. Parità pagina Services GoAlert — S1-S4 (FATTO 2026-11-12)

Migrazione `0065_services_parity.sql`. YouWorkforce non ha un oggetto "Service" unico: compone lo stesso
ruolo con ingest-token + routing + gruppo + policy. Aggiunta `incidents.forced_group_id` (override in
testa alla risoluzione del gruppo). Tutto gated `ESCALATION_MODS`.

- **S1 — Heartbeat monitor nativo:** un sistema esterno POSTa all'URL del monitor; se smette oltre il
  timeout, il watchdog apre `heartbeat_down` instradato; alla ripresa del ping l'incidente si chiude.
  È l'opposto del ping che YW fa verso GoAlert. CRUD `/api/heartbeat-monitors`, ping
  `/api/ingress/heartbeat/:token`.
- **S2 — Ingress email → allarme:** indirizzo per-tenant (`/api/email-inboxes`); un'email ricevuta via
  webhook (`/api/ingress/email`) diventa `email_alarm` instradato; tenant dal local-part, mai dal corpo;
  dedup via `+chiave`.
- **S3 — Bulk ack/close:** `/api/incidents/ack-all` e `/close-all` (ACL per sito), pulsanti in Centrale.
- **S4 — On-call-now nativo:** `/api/oncall-now-native` (chi è reperibile ora per gruppo, senza GoAlert);
  la Centrale ora lo usa (prima leggeva GoAlert → vuoto per tenant nativo).

**Verifica:** `qa/escalation-services-e2e.sh` 13/13. Regression totale: services 13 + profile 14 + rest 10
+ policy 8 + decouple 13 + loneworker 15 + unit 12. Manuale `docs/escalation-manuale.md` aggiornato.

Con questo la parità GoAlert è completa su tutte le pagine docs lette (overview, alerting, escalation
policies, schedules, rotations, profile, services, recommended-practices).
