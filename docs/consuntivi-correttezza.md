# Consuntivi — correttezza (Slice 1)

Prima tappa del completamento «Ore, spese e consuntivi». Solo modulo `consuntivi`,
migrazioni additive (`0079_timesheet_correctness.sql`): nessun listino ancora (quello è lo Slice 3).

## Cosa cambia

- **Separazione dei compiti (anti self-approval).** `approve` e `request_changes` rifiutano se
  l'approvatore è l'autore del consuntivo (`HTTP 403`). Un tenant può consentirlo esplicitamente
  (realtà piccole) impostando il config modulo `consuntivi.approval = {"allow_self_approve": true}`.
  Default: **false**.
- **ACL di sede.** `list`/`detail`/`export` applicano l'ACL per sede dell'utente (tabella
  `operator_sites`), coerente con gli incidenti: i ruoli ristretti (operatore/lavoratore/reperibile,
  anche con `timesheets.view_all` via ruolo custom) vedono solo i propri consuntivi più quelli di
  ordini nelle loro sedi. I ruoli non ristretti (responsabile/auditor/admin) con `view_all` vedono tutto.
- **Versioning ottimistico.** Colonna `version` su `timesheets`, incrementata a ogni mutazione
  (riga, spesa, transizione di stato) ed esposta in `list`/`detail` (fondamenta per il lock lato client).
- **Calcolo monetario corretto.** La valorizzazione della manodopera arrotonda al centesimo
  (half-up) per riga: `(minuti_fatturati * tariffa_cent_ora + 30) / 60`. Prima la divisione intera
  troncava i mezzi centesimi. Il CSV di export usa lo stesso arrotondamento.
- **Storico completo.** `timesheet_events` registra anche `create`, `add_line`, `add_expense`
  (oltre a submit/approve/request_changes).
- **Stati vincolati.** `CHECK` su `timesheets.state` (`bozza|in_revisione|da_correggere|approvato`).

## Invarianti mantenute

- `work_order_id` resta facoltativo: si consuntiva anche senza ordine.
- Doppia approvazione bloccata (UPDATE condizionale su `state`).
- Dopo l'approvazione il consuntivo è immutabile (nessuna riga/spesa; rettifica tracciata = Slice futuro).
- Isolamento tenant (RLS) invariato.

## Fuori da questo Slice

Allegati giustificativi alle spese, ponte intervento→consuntivo, listino/tariffario, rettifica
post-approvazione: slice successivi del piano.

## Prove

- `qa/smoke.sh` T13/T14 (34/34): totale deterministico, submit/approve, doppia approvazione, CSV.
- `qa/consuntivi-correttezza-e2e.sh` (5/5): arrotondamento half-up (838≠837), versioning, self-approval
  403, approvazione da terzo, approvato immutabile.
