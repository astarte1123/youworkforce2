# YouWorkforce

Piattaforma modulare multitenant per la sicurezza dei lavoratori (lone worker, man down, SOS),
il lavoro sul campo, l'IoT e i consuntivi. Core **Rust** (Axum/SQLx), console **Go**,
**PostgreSQL 18** con Row Level Security.

YouWorkforce è **self-contained**: non dipende da server esterni. Escalation e reperibilità,
decodifica dei dispositivi GPS/badge e piattaforma IoT sono motori nativi. L'unico servizio
esterno opzionale è il provider AI scelto dal cliente.

Stato e piano di miglioramento: [`docs/analisi-miglioramento-yw.md`](docs/analisi-miglioramento-yw.md).

## Moduli

| Area | Modulo | Cosa fa |
|---|---|---|
| Sicurezza lavoratore | `loneworker` | Centrale allarmi, SOS, check-in, profili di protezione, escalation |
| | `gps` | Dispositivi GPS e badge: Teltonika (TCP nativo), smartphone (OsmAnd), JSON HTTP. SOS, caduta, immobilità, silenzio del dispositivo → incidenti |
| IoT | `iot` | Telemetria, regole e soglie, monitoraggio, scenari attivabili (IoT Dashboard) |
| Lavoro sul campo | `interventi`, `cantieri`, `giri`, `workforce`, `consuntivi` | Ordini di lavoro, cantieri, ronde, turni, consuntivi |
| Trasversali | `assistant`, `qualifiche`, `prevenzione` | Agente AI, qualifiche e scadenze, checklist |

I servizi core (utenti, gruppi, sedi, permessi, privacy, dashboard) sono sempre attivi.

## Catena salva-vita
Dispositivo (es. Teltonika GH-5200) → decodifica nativa → evento normalizzato idempotente →
incidente (`aperto → preso_in_carico → intervento → chiuso`) → **escalation nativa** (SMS, voce,
email, Slack, webhook, con ack e chiusura) → timeline con correlazione.

Un dispositivo assegnato che smette di trasmettere apre un incidente **Perdita di contatto**
(soglia configurabile per tenant, default 10 minuti).

## Documentazione
- [`docs/manuale.md`](docs/manuale.md) — manuale utente (indice operativo)
- [`docs/console-ui-manuale.md`](docs/console-ui-manuale.md) — console: ricerca, filtri, mappe, Centrale, allarme live
- [`docs/escalation-manuale.md`](docs/escalation-manuale.md) — reperibilità e notifiche (motore nativo)
- [`docs/dispositivi-gps.md`](docs/dispositivi-gps.md) — dispositivi GPS e badge: Teltonika TCP, OsmAnd, JSON HTTP, onboarding con scanner, report
- [`docs/iot-nativo.md`](docs/iot-nativo.md) — piattaforma IoT nativa e scenari
- [`docs/gruppi-condivisi.md`](docs/gruppi-condivisi.md) — gruppi di persone condivisi
- [`docs/capability-layer.md`](docs/capability-layer.md) — permessi per ruolo e override per tenant
- [`docs/custom-roles.md`](docs/custom-roles.md) — ruoli personalizzati
- [`docs/ronde-manuale.md`](docs/ronde-manuale.md) — ronde e punzonature (QR/NFC/beacon/GPS)
- [`docs/gdpr-privacy-audit.md`](docs/gdpr-privacy-audit.md) — privacy e audit
- [`docs/campo-gruppi.md`](docs/campo-gruppi.md) — assegnazione di ordini e giri a gruppi
- [`docs/archivio/`](docs/archivio/) — documenti di progetto storici (architetture superate)

## Avvio — Docker Compose
```bash
docker compose up -d --build
# console:  http://localhost:8081
# core API: http://localhost:8080
```

## Avvio — sviluppo
```bash
# Postgres 18 (porta host 55432 per non collidere con un Postgres locale)
docker run -d --name ywdb -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=youworkforce -p 55432:5432 postgres:18
bash scripts/dev-run.sh    # core :8080 + console :8081, riavvio automatico, Ctrl-C per fermare
```
Manuale (due terminali):
```bash
cd core
export YW_DB_OWNER="postgres://postgres:postgres@localhost:55432/youworkforce"
export YW_DB_RT="postgres://app_rt:app_rt_pw@localhost:55432/youworkforce"
cargo run --release                                          # :8080 (migrazioni automatiche)
cd ../console && go build -o console.exe . && ./console.exe  # :8081 (su Windows usare il .exe)
```
Porte dei dispositivi: Teltonika TCP **5027** (`YW_TELTONIKA_PORT`), MQTT **1883**.

## Test
```bash
cd core && cargo test                 # unit test (dominio: escalation, report, moduli, …)
TEST_CLOCK=1 YW_SEED_DEMO=1 …         # core in modalità test, poi:
bash qa/run-all.sh                    # tutte le suite e2e (servono solo core + Postgres)
```
La CI (`.github/workflows/ci.yml`) esegue unit test e suite e2e a ogni push.

## Credenziali demo
| Ruolo | Tenant | Utente | Password |
|---|---|---|---|
| Superadmin | — | `admin@loneworker.eu` | `admin` |
| Tenant admin | acme | `admin@acme.loneworker.eu` | `demo` |
| Operatore centrale | acme | `operatore@acme.loneworker.eu` | `demo` |
| Lavoratore (PWA) | acme | `mario@acme.loneworker.eu` | `demo` |
| Auditor (sola lettura) | acme | `auditor@acme.loneworker.eu` | `demo` |
| Tenant secondario | globex | `admin@globex.loneworker.eu` | `demo` |

In locale lo slug del tenant si indica nella pagina di login (header `X-Tenant-Slug`); in
produzione deriva dal dominio `{slug}.loneworker.eu`.

## Struttura
```
core/        Rust: API, worker, motori nativi (escalation, edge dispositivi, IoT), migrazioni
console/     Go: server statico + proxy API; console, superadmin, PWA lavoratore
qa/          suite e2e, runbook
docs/        manuali; docs/archivio = documenti di progetto storici
contracts/   contratti tecnici (OpenAPI, schema evento, glossario, regole di isolamento)
```

## Sicurezza
- Isolamento tenant in database (RLS, ruolo runtime `NOBYPASSRLS`); tenant da dominio o da credenziale, mai dal payload.
- Password **argon2id**; sessioni persistenti su DB con scadenza e logout server-side; **CSRF** double-submit; **rate-limit** del login condiviso su DB.
- Segreti cifrati a riposo (`YW_SECRET_KEY`): la chiave del provider AI non è mai salvata né restituita in chiaro.
- Core stateless, worker con `FOR UPDATE SKIP LOCKED`, eventi live via Postgres LISTEN/NOTIFY: più istanze possono convivere.
- `TEST_CLOCK=1` espone gli hook `/api/_test/*`: **mai** in produzione.
- Da completare per la produzione: TLS e MFA.
