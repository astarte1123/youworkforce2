# Runbook operativi (bozza pilota)

## Perdita di rete (connettore sorgente)
- Sintomo: nessun nuovo evento dal badge/gateway.
- Sistema: la sessione di protezione resta attiva; il check-in **durevole** scatta comunque (timer nel core).
- Centrale: il flusso live mostra "flusso interrotto"; al ritorno riconcilia dal backend.
- Azione: verificare sorgente; procedura di sito alternativa. L'app dichiara l'assenza di connettività.

## GoAlert indisponibile
- L'outbox accumula `pending` → il worker ritenta (lease 30s, backoff).
- Dopo 6 tentativi → stato `error` + evento bus `notify_failed` (allarme operativo / degrado).
- L'incidente **resta in centrale**: nessun secondo motore di escalation viene creato.
- Canale di riserva (se configurato) indipendente, solo avviso di guasto.

## Riavvio worker / core
- L'outbox con lease scaduto viene riclaimato: nessuna scadenza o invio perso.
- Gli incidenti aperti sono preservati (stato in DB, non in memoria).

## Perdita sensore IoT
- Dashboard marca il sensore **degradato** se il connettore non garantisce aggiornamenti.
- Telemetria separata dal percorso SOS (code e risorse distinte — da misurare, T08).

## Incidente aperto durante disattivazione modulo
- `set_module(loneworker, non_abilitato)` con incidenti aperti → **409 bloccato**.
- Procedura: chiudere/migrare gli incidenti, poi aprire piano di disattivazione con audit.

## Ripristino PostgreSQL (da misurare — T07)
```bash
docker exec ywdb pg_dump -U postgres youworkforce > backup.sql     # backup
# restore su istanza pulita, poi verifica consistenza incidenti/outbox e durata.
```

## CI / run-all (regressione completa)
`qa/run-all.sh` esegue le suite e2e contro lo stack in esecuzione, raggruppate per dipendenza:
- `MODE=core` — 31 suite che richiedono solo core+Postgres (gate CI **obbligatorio**).
- `MODE=engines` — 16 suite affidabili dei motori (GoAlert/Traccar/ThingsBoard); auto-SKIP se un motore è giù. In CI è un job **best-effort** (`continue-on-error`).
- `MODE=all` (default) — core + engines.
- `RUN_SETUP=1` — include anche le 5 suite che richiedono setup esterno o un acme appena seedato
  (`goalert-e2e` serve KEY da `goalert-setup.sh`; `goalert-delivery` serve un receiver webhook;
  `goalert-p4`, `traccar-p1`, `traccar-conndown` dipendono dallo stato seed di acme + `run-*.sh`).
  Escluse di default perché non self-contained.

CI (`.github/workflows/ci.yml`): job `core` (gating, Postgres service, `YW_SEED_DEMO=1 TEST_CLOCK=1`,
`MODE=core`) + job `engines` (best-effort, avvia i container via `run-*.sh`, `MODE=engines`).
Nota: prima girava solo `smoke.sh`; ora il gate copre 31 suite. `YW_SEED_DEMO=1` è necessario
(dopo l'audit i tenant demo acme/globex si seedano solo con quel flag).
