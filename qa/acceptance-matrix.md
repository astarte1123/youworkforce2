# Matrice di accettazione — stato in questa build

Macchina di prova: Windows 11 + Docker (Postgres 18 in container), core/console nativi.
Profilo evidenza predominante: **SIMULATO**, con tre connettori **UPSTREAM REALE** (GoAlert v0.35.0, Traccar v6.17.0, ThingsBoard v3.8.1). Nessun firmware/badge hardware, nessun sito pilota.

Esegui l'evidenza automatica: `bash qa/smoke.sh` (core su :8080). Risultati sotto riferiti a quel test.

| ID | Prova | Stato | Evidenza / nota |
|---|---|---|---|
| **T01** | Onboarding dal piano di controllo | **PASS (SIM)** | `smoke.sh` crea/lista tenant; moduli cambiano senza deploy via `set_module`. Invito monouso = da fare (demo seed diretto). |
| **T02** | Separazione clienti | **PASS (SIM)** | globex non vede l'SOS di acme (RLS); token ingest errato → 401; auditor non può ack (RBAC). Enforcement in DB, non solo UI. |
| **T03** | Catena SOS | **PASS (UPSTREAM REALE)** | evento persistito → incidente → outbox → **GoAlert v0.35.0 reale** crea l'allarme sul servizio con **policy di escalation a 2 livelli** (oncall1→oncall2), correlazione nel summary, `goalert_ack` profilo UPSTREAM REALE → ack → chiusura. Evidenza: `qa/goalert-e2e.sh` (6/6). **Consegna reale** del 2° livello: **VERIFICATA via webhook** (contact-method verificato col codice consegnato al receiver; allarme core → GoAlert → escalation → receiver esterno riceve la notifica con la correlazione). Evidenza `qa/goalert-delivery.sh` (10/10). Trasporto **SMS/voce (Twilio)**: NON VERIFICATO (provider del cliente). Modalità SIMULATO resta se `GOALERT_BASE` non impostato. |
| **T04** | Check-in / man-down / disconnessione | **PARZIALE (UPSTREAM REALE)** | check-in: timer durevole scade via orologio controllabile → `mancato_checkin` anche senza nuovi dati badge. **man-down reale via Traccar**: device OsmAnd `alarm=fallDown` → Traccar v6.17.0 reale → forward → core → incidente `man_down`. Evidenza `qa/traccar-e2e.sh`. Man-down da firmware badge fisico e distinzione perdita-rete vera: **NON VERIFICATO** (serve hardware). |
| **T05** | Replay, riavvio, ritardo | **PARZIALE (SIM)** | idempotenza dimostrata (no secondo incidente). Riavvio worker con outbox pendente e riconciliazione risposta ambigua: **da scriptare** (outbox con lease già implementato). |
| **T06** | Modularità reale | **PARZIALE (SIM)** | stesso build, moduli per-tenant; disattivazione di loneworker con incidenti aperti **bloccata**. Avvio dei 4 profili completi: solo Sicurezza dimostrato end-to-end. |
| **T07** | Guasti e operazioni | **NON VERIFICATO** | runbook scritti (`runbooks.md`); ripristino PostgreSQL non ancora misurato. |
| **T08** | Separazione del carico | **NON VERIFICATO** | nessun test di carico eseguito; nessun numero vale come SLA. |
| **T09** | Distribuzione verificabile | **PARZIALE** | Compose + Dockerfile + migrazioni + nessun segreto reale nel repo. SBOM/licenze: da generare in CI. |
| **T10** | Connettori opzionali | **PARZIALE (UPSTREAM REALE)** | **Traccar v6.17.0** (event forwarding → `/api/ingress/traccar`, device per `uniqueId`) — `qa/traccar-e2e.sh` (8/8). **ThingsBoard v3.8.1** (Rule Engine REST API Call node → `/api/ingress/thingsboard`, gate soglia → `sensore_critico`) — `qa/thingsboard-e2e.sh` (8/8). Tenant sempre dal token di ingest, mai dal body. OpenDuty/Blues restano spenti e marcati. |
| **T11** | Assistente AI isolato e facoltativo | **PASS (SIM)** | Query in lettura con fonti/periodo; tenant dal contesto (no cross-tenant); azioni critiche rifiutate con rimando a procedura manuale; gating per modulo (globex→403); nessuna ipotesi su intent incerto. Provider = pianificatore locale **SIMULATO**; provider reale NON VERIFICATO. SOS completo ad AI spenta (AI fuori dal percorso). |
| **T12** | RAG YouWorker.ai | **NON IMPLEMENTATO** | Prompt 7/8 backlog; provider non contrattualizzato. |
| **T13** | Voce, turni, consuntivi | **PARZIALE (SIM)** | Consuntivi: calcolo deterministico (90m@35€/h+12,50€=65,00€), invio idempotente, nessun costo senza tariffa, CSV. **Turni** server-confirmed (inizio/fine idempotenti, un solo turno aperto) e **diario** operativo con nota **dettata** (STT lato browser Web Speech) o manuale, re-invio deduplicato, fallback offline, gating modulo. Evidenza `qa/prompt8-e2e.sh` (11/11). STT/RAG lato server: backlog. |
| **T14** | Lavoro assegnato e consuntivato | **PASS (SIM)** | Ordine creato dal pianificatore → ricevuto da mario nell'app → avviato → completato → consuntivato → approvato; RBAC, versione superata (409), doppio invio/approvazione bloccati, isolamento cross-tenant. |
| **T15** | Matrice e dry-run legacy | **NON VERIFICATO** | Prompt 10: nessun accesso al YouWorkforce esistente; richiede input (vedi `legacy-gaps.md`). |

**Regola rispettata:** nessun PASS assegnato a prova non eseguita. I NON VERIFICATO elencano la prova mancante.
