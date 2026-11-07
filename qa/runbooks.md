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
