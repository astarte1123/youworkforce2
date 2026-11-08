# Matrice di accettazione — stato in questa build

Macchina di prova: Windows 11 + Docker (Postgres 18 in container), core/console nativi.
Profilo evidenza predominante: **SIMULATO**. Nessun hardware/badge, nessun provider esterno reale, nessun sito.

Esegui l'evidenza automatica: `bash qa/smoke.sh` (core su :8080). Risultati sotto riferiti a quel test.

| ID | Prova | Stato | Evidenza / nota |
|---|---|---|---|
| **T01** | Onboarding dal piano di controllo | **PASS (SIM)** | `smoke.sh` crea/lista tenant; moduli cambiano senza deploy via `set_module`. Invito monouso = da fare (demo seed diretto). |
| **T02** | Separazione clienti | **PASS (SIM)** | globex non vede l'SOS di acme (RLS); token ingest errato → 401; auditor non può ack (RBAC). Enforcement in DB, non solo UI. |
| **T03** | Catena SOS | **PASS (SIM)** | evento persistito → incidente → outbox → GoAlert **SIMULATO** (`goalert_ack` in timeline) → ack → chiusura, con correlazione. GoAlert reale, 2 livelli, secondo reperibile: **NON VERIFICATO** (serve provider). |
| **T04** | Check-in / man-down / disconnessione | **PARZIALE (SIM)** | check-in: timer durevole scade via orologio controllabile → `mancato_checkin` anche senza nuovi dati badge. man-down fisico e distinzione perdita-rete: **NON VERIFICATO** (serve badge). |
| **T05** | Replay, riavvio, ritardo | **PARZIALE (SIM)** | idempotenza dimostrata (no secondo incidente). Riavvio worker con outbox pendente e riconciliazione risposta ambigua: **da scriptare** (outbox con lease già implementato). |
| **T06** | Modularità reale | **PARZIALE (SIM)** | stesso build, moduli per-tenant; disattivazione di loneworker con incidenti aperti **bloccata**. Avvio dei 4 profili completi: solo Sicurezza dimostrato end-to-end. |
| **T07** | Guasti e operazioni | **NON VERIFICATO** | runbook scritti (`runbooks.md`); ripristino PostgreSQL non ancora misurato. |
| **T08** | Separazione del carico | **NON VERIFICATO** | nessun test di carico eseguito; nessun numero vale come SLA. |
| **T09** | Distribuzione verificabile | **PARZIALE** | Compose + Dockerfile + migrazioni + nessun segreto reale nel repo. SBOM/licenze: da generare in CI. |
| **T10** | Connettori opzionali | **NON VERIFICATO** | ThingsBoard/OpenDuty/Blues spenti e marcati; nessuna prova di contratto reale. |
| **T11** | Assistente AI isolato e facoltativo | **PASS (SIM)** | Query in lettura con fonti/periodo; tenant dal contesto (no cross-tenant); azioni critiche rifiutate con rimando a procedura manuale; gating per modulo (globex→403); nessuna ipotesi su intent incerto. Provider = pianificatore locale **SIMULATO**; provider reale NON VERIFICATO. SOS completo ad AI spenta (AI fuori dal percorso). |
| **T12** | RAG YouWorker.ai | **NON IMPLEMENTATO** | Prompt 7/8 backlog; provider non contrattualizzato. |
| **T13** | Voce, turni, consuntivi | **PARZIALE (SIM)** | Consuntivi: calcolo deterministico (90m@35€/h+12,50€=65,00€), invio idempotente, nessun costo senza tariffa, CSV. Voce/STT/turni: backlog. |
| **T14** | Lavoro assegnato e consuntivato | **PASS (SIM)** | Ordine creato dal pianificatore → ricevuto da mario nell'app → avviato → completato → consuntivato → approvato; RBAC, versione superata (409), doppio invio/approvazione bloccati, isolamento cross-tenant. |
| **T15** | Matrice e dry-run legacy | **NON VERIFICATO** | Prompt 10: nessun accesso al YouWorkforce esistente; richiede input (vedi `legacy-gaps.md`). |

**Regola rispettata:** nessun PASS assegnato a prova non eseguita. I NON VERIFICATO elencano la prova mancante.
