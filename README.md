# YouWorkforce — release candidate (DEMO)

Piattaforma modulare multitenant per forza lavoro sul campo, sicurezza lavoratori isolati,
IoT e consuntivi. Core **Rust** (Axum/SQLx), console **Go**, **PostgreSQL 18** con RLS,
GoAlert come unico motore di escalation. LoneWorker è un modulo di prodotto.

> **Verdetto onesto di questa build: DEMO.** La catena critica (SOS → incidente → GoAlert →
> presa in carico → chiusura) è riproducibile; isolamento multitenant applicato in database.
> Integrazioni esterne (GoAlert reale, badge Traccar, ThingsBoard, YouWorker.ai) sono
> **simulate e dichiarate**. Vedi [`qa/verdetti.md`](qa/verdetti.md). Non è PILOTA né PRODUZIONE.

## Cosa è implementato
- **Multitenancy + isolamento** end-to-end: RLS su Postgres 18, ruolo runtime `NOBYPASSRLS`, contesto tenant legato alla transazione. Tenant da host (browser) o da credenziale (macchina), mai dal payload.
- **Piano di controllo** (superadmin): crea tenant, nomina admin, abilita/disabilita moduli **senza deploy**; disattivazione bloccata se toglie un percorso essenziale.
- **Catena SOS**: ingress evento normalizzato idempotente → incidente (`aperto→preso_in_carico→intervento→chiuso`) → outbox transazionale → worker → GoAlert (SIMULATO) → timeline con correlazione. Anche SOS da PWA (sessione).
- **Check-in** con timer durevole nel core (orologio controllabile per i test), che scade senza nuovi dati badge → `mancato_checkin`.
- **Centrale operativa** (SSE live), **PWA lavoratore** (SOS, check-in; documenti/voce/turni = stub marcati backlog).

## Non implementato (backlog, dichiarato)
Prompt 7 (AI), 8 (voce/diario), 9 (workforce/ordini/consuntivi completi), 10 (parità/migrazione legacy). GoAlert/Traccar/ThingsBoard/Blues/OpenDuty reali.

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
bash qa/smoke.sh            # 17 prove: T01/T02/T03/T04/T05/T06  → 17 passed, 0 failed
```

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
- Password: sha256+salt → sostituire con **argon2id**. Sessioni in memoria → store persistente.
- Nessun TLS/MFA/CSRF configurati nel profilo locale; richiesti per il pilota.
- `TEST_CLOCK=1` espone `/api/_test/advance`: **disabilitare** (`TEST_CLOCK=0`) fuori dai test.
- GoAlert SIMULATO: un ack locale non prova la consegna reale.
