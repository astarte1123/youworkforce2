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

## Correzione, tariffe e ponte dall'intervento (CA-8, CA-14, CA-18 · mig `0139`, 25/11/2026)

- **Correzione (CA-8).** Un consuntivo in *bozza* o *da correggere* si corregge dal proprietario:
  `POST/DELETE /api/timesheets/:id/lines/:lid`, `POST/DELETE /api/timesheets/:id/expenses/:eid`
  (versione +1 e voce nello storico). La nota di chi rimanda va in `review_note` (prima
  sovrascriveva `note`, che porta anche i materiali dell'intervento); lo **storico**
  (`history`: creazione, righe, invio, correzioni richieste con la nota, tariffe, approvazione)
  è nel dettaglio e nella console.
- **Tariffe (CA-14).** Chi consuntiva **non sceglie la propria tariffa**: la tariffa indicata vale
  solo per chi approva, pianifica o vede i costi; per gli altri si usa la tariffa oraria di
  manodopera del **listino** (prima voce attiva «manodopera» a ore, anche per la riga generata
  dall'intervento), altrimenti la riga resta *da valorizzare* e **chi approva la fissa** durante la
  revisione (`POST /api/timesheets/:id/lines/:lid/rate`, separazione dei compiti). Modificare una
  riga non ne cambia la tariffa se non la si può fissare.
- **Valori (CA-14).** Minuti 1–1440, spese > 0, valuta di 3 lettere; vincoli anche nel DB
  (`NOT VALID` se esistono righe storiche fuori regola, validati quando possibile). Il **totale è
  nella valuta del tenant**; altre valute restano a parte (`total.by_currency`,
  `total.mixed_currency`), mai sommate.
- **Ponte dall'intervento (CA-18).** Il consuntivo si genera solo a intervento **completato**
  (409 altrimenti); se i minuti dell'intervento cambiano e il consuntivo è ancora modificabile la
  riga si riallinea (`line_updated`).
- Prove: `qa/timesheet-corrections-e2e.sh` **22/22**; aggiornati `consuntivi-bridge-e2e` (6/6),
  `consuntivi-correttezza-e2e` (5/5), `smoke.sh` (35/35): la tariffa la fissa chi approva.

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
