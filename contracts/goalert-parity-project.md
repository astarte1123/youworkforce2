# Progetto: GoAlert Parity — chiusura gap YouWorkforce ↔ GoAlert

Stato: COMPLETO (e2e 15/15 su GoAlert vivo) · Owner: core/api + console · Motore: GoAlert bundled (:8082)
Principio guida: **tutto configurabile da YouWorkforce, in modo semplice.** L'admin del
tenant non apre mai la console GoAlert. Ogni funzione ha un default sicuro e si attiva
con un click. I segreti restano write-only/mascherati.

## Contesto
Audit della documentazione GoAlert (23 pagine, goalert.me/docs) confrontata col codice
(`core/src/goalert.rs`, `sync_goalert` in `api.rs`, `worker.rs`). YouWorkforce già pilota
GoAlert per: apri/chiudi alert (Generic API + dedup, outbox at-least-once), provisioning
EP+service+key+heartbeat, sync Gruppi→EP (rotation/step/repeat), recapiti EMAIL+SMS,
config trasporti (Twilio/SMTP/Slack/Mailgun). Mancano 12 funzioni che GoAlert offre.

## Gap → soluzione (ordine di chiusura)

### 🔴 Sicurezza della catena
1. **Ladder di notifica con fallback vocale.** Oggi solo regola SMS@0. GoAlert raccomanda
   SMS subito + VOICE dopo N minuti se non si risponde.
   → `sync_goalert` crea, per ogni membro: regola immediata (SMS/EMAIL @0) + regola VOICE
   @`voice_fallback_min`. Config tenant: toggle `voice_fallback_on` (default ON) + minuti
   (default 5).
2. **Contact method VOICE + verifica/test.** Oggi solo EMAIL+SMS, non verificati.
   → `sync_goalert` crea anche VOICE (se Twilio attivo + telefono E.164). Endpoint
   interattivi: `sendContactMethodVerification` / `verifyContactMethod` / `testContactMethod`.
   UI: elenco recapiti per membro con stato verificato + bottoni "Invia codice / Verifica / Prova".
3. **Schedule con copertura a fasce orarie.** Oggi solo rotation piatta 24/7.
   → Config per gruppo `coverage_mode`: `24_7` (default, invariato) oppure `orari`
   (finestra start–end + giorni). In `orari`, `sync_goalert` crea una Schedule con regole
   (ScheduleRule start/end/weekdayFilter) che punta alla rotation/utenti, e lo step EP
   punta alla Schedule.
4. **Override (assenze/sostituzioni).** Assente.
   → Endpoint `createUserOverride`/`updateUserOverride`/delete sulla schedule del gruppo.
   UI "Assenze e sostituzioni": membro, dal–al, (opzionale) sostituto.

### 🟠 Operatività
5. **Sync ack/chiusura bidirezionale.** Oggi solo YW→GoAlert. Se il reperibile fa ack via
   SMS/voce, l'incidente YW resta aperto.
   → Worker: poll periodico degli incidenti aperti con `goalert_alert_id`; legge
   `alert.status`; `StatusAcknowledged`→timeline `goalert_acked` + stato preso-in-carico;
   `StatusClosed`→chiusura incidente. Automatico, zero config.
6. **Maintenance Mode.** Assente.
   → Endpoint `updateService(maintenanceExpiresAt)` su tutti i servizi del tenant.
   UI: bottoni "Manutenzione 1h/2h/4h" + "Termina". Durante la finestra niente escalation.
7. **Alert Status Updates.** Assente.
   → `enableStatusUpdates:true` sul contact method primario (auto, toggle tenant
   `status_updates_on`, default ON): chi è on-call è avvisato se un altro modifica l'alert.

### 🟡 Comfort
8. **Calendario reperibilità (iCal).** Assente.
   → Endpoint YW-native `GET /oncall/calendar.ics?token=…` generato dai turni YW (non
   dipende da GoAlert). UI: "Il mio calendario" con URL da copiare in Apple/Google Calendar.
9. **On-Call Notifications su Slack.** Assente.
   → `setScheduleOnCallNotificationRules` (canale Slack + orario o on-change) quando il
   gruppo ha una schedule e un canale Slack configurato.
10. **Canale Slack come target di step EP.** Assente.
    → Campo opzionale `slack_channel_id` per gruppo → aggiunto ai target dello step.
11. **Rotation: handoff time + giorno.** Oggi solo type/shiftLength/tz.
    → Config gruppo `handoff_time` + `handoff_day`; `sync_goalert` imposta `start` della rotation.
12. **Labels sui servizi.** Assente.
    → `setLabel` automatico: `youworkforce/tenant`=slug, `youworkforce/group`=nome gruppo.

## Modello dati (migrazione 0050)
- `alarm_groups`: +`coverage_mode text default '24_7'`, +`cov_start text`, +`cov_end text`,
  +`cov_days text` (7 char bitmask lun..dom), +`handoff_time text`, +`handoff_day int`,
  +`slack_channel_id text`, +`slack_notify text` ('off'|'onchange'|'HH:MM'),
  +`goalert_schedule_id text`.
- `tenant_oncall_cfg(tenant_id pk, voice_fallback_on bool default true,
  voice_fallback_min int default 5, status_updates_on bool default true)`.
- `oncall_ical_tokens(tenant_id, user_id, token, created_at)` per il feed iCal per-utente.

## Endpoint (tutti tenant_admin, modulo loneworker; iCal per-utente)
- esteso `POST /api/groups/:id/oncall` — accetta anche coverage/handoff/slack.
- `GET/POST /api/oncall/settings` — voice fallback + status updates (tenant).
- `GET /api/oncall/contacts` — recapiti GoAlert dei membri con stato.
- `POST /api/oncall/contacts/:cmid/verify` · `/code` · `/test`.
- `GET/POST/DELETE /api/oncall/overrides` — assenze/sostituzioni.
- `POST /api/oncall/maintenance` {hours|end} — manutenzione servizi.
- `GET /api/oncall/calendar/url` + `GET /api/oncall/calendar.ics?token=…` (pubblico col token).

## UI (console tenant_admin)
Tutto dentro **Gruppi → Reperibilità**: la card gruppo guadagna "Copertura" (24/7 | orari)
e "Avanzate" (handoff, Slack). Nuova sezione collassabile **Reperibilità avanzata**:
impostazioni tenant (fallback vocale, status updates), recapiti+verifica, assenze,
manutenzione, calendario. Un solo pulsante **Sincronizza con GoAlert** applica tutto.

## Rollout / test
- QA `qa/goalert-parity-e2e.sh` contro GoAlert vivo (:8082): verifica ladder vocale,
  schedule+regole, override, maintenance, labels, status updates, ack-sync simulato.
- Nessuna rottura del flusso esistente: `24_7` = comportamento attuale.

## Deliverable finale
`docs/goalert-manuale.md` — manuale admin: come configurare e come usare, funzione per funzione.

## Task — tutte COMPLETE
- [x] 0050 migrazione (colonne + tabelle + GRANT app_rt)
- [x] #11 handoff/day rotation in sync (`rotation_start`)
- [x] #1 ladder vocale + #2 VOICE cm in sync (`ensure_contact_methods`)
- [x] #7 status updates in sync (`enableStatusUpdates`)
- [x] #12 labels in sync (`setLabel`)
- [x] #3 schedule coverage (sempre wrappata) + #10 slack step target + #9 on-call slack
- [x] #2 endpoint verify/code/test + UI recapiti
- [x] #4 endpoint override + UI assenze
- [x] #6 endpoint maintenance + UI
- [x] #8 iCal endpoint YW-native + UI
- [x] #5 worker ack-sync (`goalert_ack_sync`, ~14s)
- [x] UI tenant settings + wiring Gruppi (Reperibilità avanzata)
- [x] QA e2e verde — `qa/goalert-parity-e2e.sh` 15/15 su GoAlert vivo
- [x] docs/goalert-manuale.md

## Verifica (e2e 15/15, GoAlert :8082, tenant globex)
VOICE cm + ladder @3 · schedule+regole copertura · handoff rotation · labels · SMS primario ·
recapiti via endpoint · override crea/elenca/elimina · maintenance on/off · iCal servito.
Regressione: smoke 34/34, loneworker 25/25, goalert-config 6/6. Nessuna rottura del flusso
esistente (default `24_7`). Twilio ripristinato OFF sul motore condiviso dopo i test.
