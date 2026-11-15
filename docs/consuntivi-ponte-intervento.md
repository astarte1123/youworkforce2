# Consuntivi — ponte intervento → consuntivo (Slice 2)

Secondo slice di «Ore, spese e consuntivi»: **un solo inserimento**. Il tecnico non ricompila a
mano ore e materiali: li prende dall'esecuzione dell'intervento. Migrazione additiva
(`0080_timesheet_from_order.sql`).

## Cosa fa

Endpoint: `POST /api/orders/:id/timesheet` (richiede moduli `interventi` + `consuntivi` attivi).

- Genera (o **riusa**) una **bozza** di consuntivo per l'**assegnatario** dell'ordine.
- Prefilla una riga `labor` dai **minuti lavorati** dell'intervento (`work_minutes`); la data è quella
  di inizio/arrivo. La **tariffa resta da valorizzare** (listino = Slice 3): la riga esce con
  `missing_rate`.
- I **materiali** dell'intervento finiscono nella **nota** del consuntivo (riepilogo leggibile).
- **Idempotente**: ri-eseguire non duplica. Vincolo univoco `(tenant, source_order_id,
  source_user_id, 'labor')` → la riga labor dello stesso tecnico per lo stesso ordine esiste una volta.
  Risposta: `{timesheet_id, created, line_added}` (entrambi `false` al secondo giro).

## Autorizzazione e validazioni

- Può chiamarlo l'**assegnatario** dell'ordine o un **pianificatore** (`plan`).
- Ordine non assegnato a una persona → `400`.
- Nessun minuto lavorato → `400` (niente da consuntivare).
- Proprietà del dato: *Interventi* resta padrone di rapporto/esito/minuti/materiali; *Consuntivi* della
  riga e (in Slice 3) della valorizzazione. Il ponte copia, non sposta.

## UI

PWA lavoratore (pagina «Oggi», modale esecuzione): bottone **«Genera consuntivo dall'intervento»**
quando il modulo consuntivi è attivo. Precompila ore e materiali; la tariffa si valorizza nel consuntivo.

## Fuori da questo slice

Valorizzazione da listino, spesa condivisa su più tecnici (serve il modello a visite), rettifica
post-approvazione: slice successivi.

## Prove

`qa/consuntivi-bridge-e2e.sh` (5/5): generazione con riga prefillata (minuti + materiali in nota,
tariffa da valorizzare), idempotenza (nessun duplicato), ordine non assegnato → 400.
Regressioni: `qa/smoke.sh` 34/34, `qa/consuntivi-correttezza-e2e.sh` 5/5.
