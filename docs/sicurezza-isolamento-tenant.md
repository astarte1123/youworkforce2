# Sicurezza — isolamento multi-tenant degli allarmi

Gli allarmi (man_down, SOS, sicurezza) e la loro gestione sono **confinati al singolo tenant**:
ogni tenant vede e gestisce solo i propri. Nessuna azione di un tenant tocca gli allarmi di un altro.

## Doppia barriera

1. **RLS (Row Level Security).** Ogni handler apre la connessione con `db::begin_tenant(rt_pool, tid)`
   che imposta il tenant corrente; le policy `tenant_isolation` filtrano righe per `tenant_id`. Il
   ruolo applicativo `app_rt` **non** è owner né `BYPASSRLS`, quindi è soggetto a RLS.
2. **`tenant_id` esplicito nelle mutazioni.** Le UPDATE sugli incidenti — singole e **bulk**
   (`close-all`, `ack-all`) — filtrano sempre anche `AND tenant_id = <tenant della sessione>`. Così
   l'isolamento regge **anche** se la RLS fosse disattivata o il ruolo DB mal configurato (es. un
   `owner_pool` che gira come superuser con `BYPASSRLS`). Difesa in profondità su operazioni
   distruttive.

## Perché il bulk era il punto critico

`incidents_close_all` / `incidents_ack_all` chiudevano/prendevano in carico «tutti gli aperti»
basandosi solo sulla RLS (`WHERE state<>'chiuso'`, senza `tenant_id`). Se la RLS non fosse stata
effettiva (ruolo privilegiato), un «Chiudi tutti» in un tenant avrebbe potuto chiudere gli allarmi
di tutti i tenant. Ora il `tenant_id` esplicito lo rende impossibile a prescindere dalla RLS.

## Nota su FORCE RLS

Non si usa `FORCE ROW LEVEL SECURITY`: il worker di sistema (outbox, escalation, watchdog) opera su
`owner_pool` iterando legittimamente tutti i tenant, e lo fa già con `tenant_id` esplicito in ogni
query. Forzare la RLS sull'owner romperebbe quei processi senza aggiungere sicurezza rispetto al
filtro esplicito già presente.

## Prove

`qa/tenant-isolation-e2e.sh` (4/4): due tenant con allarmi aperti; il `close-all` di uno chiude solo
i suoi, l'altro resta intatto; la chiusura singola cross-tenant (per id) è bloccata. Regressioni:
`qa/smoke.sh` 34/34 (incl. T02 «globex non vede SOS di acme»).
