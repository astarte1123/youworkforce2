# Progetto: Escalation Engine nativo YouWorkforce (drop GoAlert)

Stato: PROPOSTA (plan-only) · Owner: core · Obiettivo: sostituire GoAlert con un motore
di escalation/on-call/consegna **nativo**, senza perdere affidabilità sulla catena di sicurezza.
Prerequisito culturale: **un SOS non deve MAI sparire.** Ogni scelta progettuale è subordinata
a questo. GoAlert resta acceso finché il nativo non prova parità a specchio (shadow).

Contesto: oggi YouWorkforce **pilota** GoAlert (vedi [[goalert-parity]], `goalert.rs`,
`sync_goalert`). GoAlert resta il "cervello" (calcolo on-call, esecuzione escalation, ladder
notifiche, consegna Twilio/SMTP/Slack, ack a due vie, heartbeat). Questo progetto porta quei
motori dentro YouWorkforce.

## 1. Cosa va riscritto (i motori di GoAlert)
1. **On-call resolver** — dato rotation + schedule + regole orarie + override + temporary,
   calcola *chi è reperibile in un istante t* (e nel futuro, per il calendario).
2. **Escalation engine** — macchina a stati per alert: step ordinati, attesa per-step, repeat,
   dedup, stop su ack/close.
3. **Notification scheduler** — per ogni destinatario, ladder temporizzata (SMS @0, VOICE @N,
   EMAIL…), con retry e backoff.
4. **Delivery adapters** — consegna reale: Twilio (SMS + voce TwiML/IVR), SMTP, Slack, webhook.
5. **Ack bidirezionale** — webhook inbound: risposta SMS (ACK/STOP), IVR a tasti in chiamata.
6. **Heartbeat/watchdog** — monitor "sorgente silente" (già in parte nostro, da generalizzare).
7. **Verifica recapiti** — invio codice + conferma (oggi delega a GoAlert).

## 2. Architettura proposta
Tutto dentro `core` (Rust/Axum), stesso DB, stessa outbox at-least-once. Niente nuovi runtime.

```
incidente ──▶ escalation_engine ──▶ oncall_resolver ─▶ destinatari
                    │                                      │
                    ▼                                      ▼
             escalation_state (DB)                notification_scheduler
                    ▲                                      │
          ack/close │                                      ▼
                    └──────── inbound webhook ◀── delivery_adapters (Twilio/SMTP/Slack)
                                                           │
                                                   delivery_log (DB, idempotente)
```

- **Idempotenza**: ogni notifica ha una chiave (`alert_id`+`step`+`cm_id`+`attempt`); il drain
  non consegna due volte. Riuso del pattern `outbox` esistente (già robusto per `goalert_*`).
- **Clock controllabile**: il motore usa `st.clock` (come lo sweep check-in) → testabile e
  deterministico negli e2e.
- **Separazione**: *decisione* (chi/quando) separata da *consegna* (come). La decisione è pura
  e testabile senza rete; la consegna è un adapter mockabile.

## 3. Modello dati (nuove tabelle, prefisso esc_)
- `esc_contact_methods(id, tenant_id, user_id, kind[sms|voice|email|slack|webhook], value,
  verified_at, status_updates bool)` — recapiti nativi (rimpiazza i contactMethods GoAlert).
- `esc_notification_rules(id, contact_method_id, delay_s)` — ladder per utente.
- `esc_policies(id, tenant_id, group_id, repeat, …)` + `esc_steps(id, policy_id, pos, delay_s)`
  + `esc_step_targets(step_id, kind[user|rotation|schedule|slack], ref_id)`.
- `esc_schedules(id, tenant_id, tz)` + `esc_schedule_rules(schedule_id, target_kind, target_ref,
  start_clock, end_clock, weekday_mask)` + `esc_rotations(id, type, shift_len, handoff_start, tz)`
  + `esc_rotation_members(rotation_id, user_id, pos)` + `esc_overrides(schedule_id, start, end,
  add_user, remove_user)` + `esc_temp_schedules(…)`.
- `esc_alert_state(alert_id, incident_id, step, next_fire_at, repeat_count, status
  [unack|ack|closed])` — stato della macchina.
- `esc_delivery_log(id, alert_id, step, contact_method_id, attempt, channel, provider_ref,
  state[queued|sent|delivered|failed|acked], at)` — audit + idempotenza.
- `esc_heartbeats(id, tenant_id, name, timeout_s, last_ping_at)`.
Riuso: `alarm_groups`, `alarm_routing`, `oncall`, `incidents`, `incident_timeline`, `outbox`.

## 4. On-call resolver (il pezzo più ostico)
Funzione pura `on_call_at(schedule, t) -> Vec<user>`:
1. parte dalle `esc_schedule_rules` (target + finestra oraria + giorni) nel fuso della schedule;
2. risolve i target `rotation` → membro attivo nel turno a `t` (handoff, shift_len, start);
3. applica gli `esc_overrides` (remove/replace/add) e i `temp_schedules` (sostituiscono tutto);
4. ritorna l'insieme on-call. Deterministico, nessuna rete → **test a tabella** (property-based
   su fusi, DST, confini di turno, override sovrapposti).
DST e fusi: usare `chrono-tz`; i confini turno sono il bug-magnet classico → copertura test alta.

## 5. Delivery adapters
- **Twilio SMS**: REST `Messages`; `StatusCallback` → `esc_delivery_log`. Risposta inbound
  (`/api/ingress/twilio/sms`): parsing ACK/STOP → ack/close alert. Firma Twilio validata.
- **Twilio Voice**: `Calls` con TwiML (`<Gather>` DTMF): "premi 1 per prendere in carico".
  Webhook `/api/ingress/twilio/voice` → DTMF → ack. Fallback se non risponde (passo successivo).
- **SMTP**: invio diretto (lettly/async-smtp); bounce best-effort.
- **Slack**: `chat.postMessage` (step target + on-call notification); opz. bottoni ack (Block Kit
  → `/api/ingress/slack/action`).
- **Webhook generico**: POST firmato (per integrazioni cliente).
Tutti gli adapter: timeout, retry con backoff, marcati **critici** nell'outbox (come oggi i
`goalert_*`) → un provider down non perde l'SOS, lo ritenta.

## 6. Verifica recapiti (nativa)
`sendVerification` (codice OTP via SMS/voce/email) + `verifyCode`. Stato `verified_at` sul
contact method. Stessa UX già costruita in [[goalert-parity]] (invia codice / inserisci / prova),
solo con backend nostro.

## 7. Affidabilità (requisito #1: SOS mai perso)
- Outbox at-least-once + idempotenza su `esc_delivery_log` (nessun doppio invio, nessun buco).
- Il motore gira nel worker con lo stesso loop del drain; stato in DB, riavvio-safe.
- **Dead-man**: se il worker non tocca `esc_alert_state` entro X, un watchdog esterno (heartbeat
  verso un endpoint di monitoring) allerta. (Oggi GoAlert faceva da guardia reciproca.)
- Multi-istanza: lock ottimistico su `next_fire_at` (SKIP LOCKED) per non doppiare l'escalation.

## 8. Roadmap (GoAlert in parallelo fino a parità)
**Fase 0 — Decisione pura, nessuna consegna. ✅ FATTO** (`core/src/escalation.rs`). On-call resolver
(`Rotation::active_at`, `Schedule::on_call_at` con regole di copertura a fasce incl. wrap notturno,
`Override` replace/remove/add anche fuori copertura) + escalation state machine (`tick` → FireStep/
Wait/Exhausted/Stop con step/delay/repeat/ack). 7 test a tabella verdi (`cargo test escalation::`).
Nessun effetto esterno, modulo non wirato (GoAlert resta il motore). Limite noto FASE 0: fasce in UTC
(fuso/DST da applicare a monte / fase successiva). Prossimo: Fase 1 shadow (calcolo in parallelo a
GoAlert + diff).

**Fase 1 — Shadow mode. ✅ IN ESERCIZIO** (mig 0052 `esc_shadow_log`). Il worker (`escalation_shadow_scan`,
~ogni 5 min) e il trigger manuale (`POST /api/escalation/shadow/run`) calcolano l'on-call nativo
(`esc_build_schedule` dalla stessa config di `sync_goalert` → `escalation::Schedule::on_call_at`) e lo
confrontano con l'on-call corrente di GoAlert (`service.onCallUsers.userName` = email), loggando match/diff.
Report: `GET /api/escalation/shadow` (match-rate 7gg + ultime divergenze). **Nessuna consegna**, GoAlert
resta il motore.

**DIVERGENZA WEEKLY — RISOLTA (mig 0053 `esc_rotation_state`).** La prima iterazione shadow (formula
pura `shift_number(now) % N`) divergeva sui gruppi 'turni' weekly (native idx1 vs GoAlert
activeUserIndex=0). **Root cause** (studiata la sorgente GoAlert: `schedule/rotation/rotation.go` +
`engine/rotationmanager` `RotMgrStart`/`calcAdvance`): **l'indice GoAlert NON è funzione dello `start`**,
è *stato mutabile* — alla CREAZIONE della rotation `RotMgrStart` semina `position=0`, `shift_start=now()`,
poi `calcAdvance` fa `position=(position+1)%N` a ogni boundary. Lo `start` definisce solo la *griglia*
dei handoff, non l'indice. Formula corretta: `index = (shift_number(now) − shift_number(creazione)) mod N`
(non `shift_number(now) % N`): per una rotation creata nella settimana corrente → 0, come GoAlert.
**Fix:** `escalation::RotationState{position,shift_start}` + `seed(creato)`/`advanced_to` (replica
`calcAdvance`: +1 per boundary, reset su partecipante rimosso) e `Rotation::start_time/end_time` (replica
`StartTime/EndTime`). Lo shadow carica/semina lo stato in `esc_rotation_state`: alla prima osservazione
semina dal **ground-truth GoAlert** (indice del membro on-call corrente), poi avanza indipendente sulla
stessa griglia. Robustezza: se GoAlert non ha ancora un on-call (warm-up engine post-sync) non semina un
indice inventato → native vuoto = match, niente falsa divergenza. **3 nuovi unit test** (semina a creazione
= idx0; avanza +1 per boundary con wrap; reset su rimozione) → `cargo test escalation::` 10/10.
**Verificato live su globex** (goalert riabilitato temporaneamente, poi ripristinato): rotation weekly
**multi-membro** (admin/qa-b/qa-c, start 2024-01-01) → GoAlert on-call = admin (idx0), native = admin →
**match**, `match_rate 1.0`. Discriminatore: la formula pura avrebbe dato `149%3=2` (qa-c) ≠ GoAlert →
il vecchio codice divergeva, il nuovo combacia. Artefatti di test ripuliti.

**RESIDUO FASE 1 — CHIUSO.** Override (assenze) ora modellati lato nativo (`esc_goalert_overrides`,
commit 8130b27). **Verifica LIVE su globex (2026-11-11):** override reale di rimozione (assenza admin
per finestra) via `POST /api/oncall-overrides` → GoAlert on-call `{}`, native `{}`, **match, overrides=1**
(2 giri stabili); rimosso l'override → entrambi `{admin}`, match. Pre-fix il native avrebbe mostrato
`{admin}` contro GoAlert `{}` = divergenza. Nota minore: subito dopo `sync-goalert` GoAlert ha un transiente
di warm-up (`onCallUsers` vuoto per 1 ciclo) → falso `match=f` sui gruppi fixed-user/'tutti'; il guard
warm-up oggi copre solo le rotation, va esteso ai fixed-user.

**Fase 2 — Consegna in ombra. ✅ PRIMA ITERAZIONE** (mig 0054 `esc_alert_state`+`esc_delivery_log`,
commit 9bc7196). Il motore nativo ora DECIDE e CONSEGNA in ombra: per ogni incidente aperto
instradato a un gruppo (`alarm_routing`), risolve l'on-call nativo (riusa resolver stateful +
override di FASE 1), usa `escalation::tick` per scegliere la rung dovuta e la consegna al **sink di
prova** (`YW_ESC_SINK_URL`; dry-run se assente). Modulo nuovo `core/src/delivery.rs` (parte pura
testata, 7 test): `Channel/Rung`, `ladder_for` (SMS@0, VOICE@`voice_fallback_min` da
`tenant_oncall_cfg`, email fallback), `request_body` modellato come Twilio REST `Messages`/`Calls`
(TwiML `<Gather>` DTMF pronto per il cutover), `delivery_key`, `deliver_to_sink`. **Idempotenza**:
UNIQUE `(tenant,incident,step,channel,to)` su `esc_delivery_log` → ogni rung consegnata una sola
volta anche tra riavvii (claim `INSERT … ON CONFLICT DO NOTHING RETURNING`). `esc_alert_state` =
macchina a stati per incidente; ack/chiusura incidente → `tick` Stop ferma l'escalation. Worker
scan ~ogni 30s. API: `GET /api/escalation/delivery` (conteggi+ultime), `POST …/run`. Timeline
`esc_shadow_delivery`. **Nessun carrier reale, GoAlert resta il motore.** 22/22 test verdi.
**RESIDUO FASE 2:** (a) Twilio REST **live** (gate `YW_ESC_TWILIO_LIVE`) verso carrier vero via sink
→ carrier; (b) **ack inbound** firmato (`/api/ingress/twilio/sms|voice` → ack/close); (c) SMTP+Slack
adapter; (d) `repeat` della policy (ora 0); (e) verifica LIVE su globex con sink reale.

**Fase 3 — Cutover per-tenant.** Flag `tenant.escalation_engine = native|goalert` (default
goalert). Si accende `native` su globex, poi tenant pilota, con rollback immediato al flag.
GoAlert resta provisionato come rete di sicurezza.

**Fase 4 — Spegnimento GoAlert.** Quando tutti i tenant sono `native` e stabili: si rimuove il
provisioning, `goalert.rs` e la dipendenza dal container. Si aggiorna il manuale.

**Fase 5 — Pulizia.** Deprecazione schema goalert_*; i dati on-call vivono solo in `esc_*`.

## 9. Rischi e mitigazioni
- **On-call errato su DST/confine turno** → resolver puro + property tests + shadow diff.
- **Mancata consegna Twilio** (cred/limiti/carrier) → outbox critico, retry, status callback,
  fallback vocale, alert "trasporto giù".
- **Doppio invio in multi-istanza** → SKIP LOCKED + idempotenza delivery_log.
- **Regressione silenziosa** → la fase shadow è bloccante: nessun cutover senza 0-diff.
- **Superficie di sicurezza nuova** → mantenere GoAlert come fallback attivabile a flag fino a
  fiducia piena.

## 10. Cosa NON cambia
UI admin (già astratta da [[goalert-parity]]: Gruppi + Reperibilità avanzata), catena incidenti,
outbox, calendario iCal (già nativo), principio "tutto configurabile da YouWorkforce".
Per l'admin il cambio è invisibile: stesse schermate, motore diverso sotto.

## 11. Stima grossolana
- Fase 0–1 (resolver + engine + shadow): grande, è il cuore. Molto test.
- Fase 2 (Twilio voce/SMS + inbound ack): media-grande, rischio su TwiML/IVR e firma webhook.
- Fase 3–5 (cutover, cleanup): piccola ciascuna ma va fatta con cautela per tenant.
Settimane, non giorni. Catena di sicurezza → niente scorciatoie, shadow obbligatorio.

## 12. Task (alla partenza)
- [ ] Fase 0: crate/modulo `escalation` (resolver puro + state machine) + test a tabella
- [ ] mig esc_* (contact methods, rules, policies, steps, schedules, rotations, overrides, state, delivery_log, heartbeats)
- [ ] Fase 1: shadow runner nel worker + esc_shadow_log + diff GoAlert vs nativo (globex)
- [ ] Fase 2: adapter Twilio SMS/voce (TwiML+DTMF) + SMTP + Slack + inbound webhooks firmati
- [ ] verifica recapiti nativa (OTP) + wiring UID esistente
- [ ] Fase 3: flag `escalation_engine` per-tenant + cutover globex + rollback
- [ ] parità e2e nativo (mirror di `goalert-parity-e2e.sh`) senza GoAlert
- [ ] Fase 4: spegnimento GoAlert + rimozione provisioning + aggiornamento `docs/goalert-manuale.md`
- [ ] Fase 5: deprecazione schema goalert_*
