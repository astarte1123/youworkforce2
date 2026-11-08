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
- **Piano di controllo** (superadmin): crea tenant, nomina admin, abilita/disabilita moduli **senza deploy** (switch in UI); disattivazione bloccata se toglie un percorso essenziale.
- **Catena SOS**: ingress evento normalizzato idempotente → incidente (`aperto→preso_in_carico→intervento→chiuso`) → outbox transazionale → worker → GoAlert (SIMULATO) → timeline con correlazione. Anche SOS da PWA (sessione).
- **Check-in** con timer durevole nel core (orologio controllabile per i test), che scade senza nuovi dati badge → `mancato_checkin`. In PWA con countdown live.
- **Ordini di lavoro** (Prompt 9): bozza→assegnato→in_corso→completato→annullato, con concorrenza ottimistica (versione), RBAC e isolamento.
- **Consuntivi** (Prompt 9): bozza→in_revisione→da_correggere→approvato; righe ore + spese, calcolo deterministico in centesimi, doppia approvazione bloccata, **export CSV**.
- **Assistente AI** (Prompt 7, sola lettura): widget NL nella console con strumenti tipizzati eseguiti da Rust (check-in scaduti, sensori offline, storia incidente, ordini, moduli degradati). Tenant/permessi dal contesto autenticato; azioni critiche rifiutate; provider intercambiabile (ora pianificatore locale **SIMULATO**). Si abilita per tenant dal superadmin.
- **UI**: design system completo (chiaro/scuro, responsive), console SPA (dashboard, centrale live, ordini, consuntivi, moduli) con drawer e toast; PWA lavoratore mobile-first.

## Non implementato (backlog, dichiarato)
Prompt 7 (AI), 8 (voce/diario/documenti), 10 (parità/migrazione legacy). GoAlert/Traccar/ThingsBoard/Blues/OpenDuty reali.

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
**Consegna reale** (SMS/voce/webhook) del 2° livello = da cablare per sito (trasporto di test).

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
