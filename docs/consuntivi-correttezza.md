# Consuntivi — regole di correttezza

Modulo `consuntivi` («Ore, spese e consuntivi»). Qui le regole che rendono un consuntivo
affidabile: chi può fare cosa, come si calcolano gli importi, da dove vengono le tariffe e come
il consuntivo si lega a interventi e registro delle ore. Le parti sono arrivate in più tappe:
Slice 1 (mig `0079`), ponte dall'intervento (mig `0080`), correzione e valori (mig `0139`),
listino unico per qualifica (mig `0156`–`0157`), registro unico delle ore (mig `0158`).

## Stati e flusso

`bozza` → `in_revisione` → `approvato`, con ritorno `in_revisione` → `da_correggere` → `in_revisione`.
`CHECK` su `timesheets.state` (mig `0079`).

| Metodo | Rotta | Chi | Azione |
|---|---|---|---|
| POST | `/api/timesheets` | utente del tenant | crea una bozza propria `{work_order_id?, note?}` |
| GET | `/api/timesheets` | vedi ACL | elenco |
| GET | `/api/timesheets/:id` | vedi ACL | dettaglio con righe, spese, totali, storico |
| POST | `/api/timesheets/:id/lines` | proprietario, in bozza/da correggere | aggiungi riga `{activity, work_date, minutes, rate_cents?, currency?}` |
| POST/DELETE | `/api/timesheets/:id/lines/:lid` | proprietario, in bozza/da correggere | modifica / elimina riga |
| POST | `/api/timesheets/:id/lines/:lid/rate` | chi approva, in revisione | fissa la tariffa `{rate_cents}` |
| POST | `/api/timesheets/:id/expenses` | proprietario, in bozza/da correggere | aggiungi spesa `{description, amount_cents, currency?, has_receipt?}` |
| POST/DELETE | `/api/timesheets/:id/expenses/:eid` | proprietario, in bozza/da correggere | modifica / elimina spesa |
| POST | `/api/timesheets/:id/submit` | proprietario | invia in revisione |
| POST | `/api/timesheets/:id/approve` | capability `approve` | approva |
| POST | `/api/timesheets/:id/request-changes` | capability `approve` | rimanda `{note}` → `da_correggere` |
| GET | `/api/timesheets/:id/export.csv` | vedi ACL | esporta CSV |
| POST | `/api/orders/:id/timesheet` | assegnatario o pianificatore | genera il consuntivo dall'intervento |
| GET | `/api/timesheets/:id/ai-summary` | vedi ACL | sintesi AI (funzione AI da accendere) |

Tutte le rotte verificano che il modulo `consuntivi` sia attivo (CA-9); il ponte dall'intervento
richiede anche `interventi`.

## Regole (Slice 1, mig `0079`)

- **Separazione dei compiti (anti self-approval).** `approve`, `request-changes` e la tariffa
  fissata in revisione rifiutano se chi agisce è l'autore del consuntivo (`HTTP 403`). Un tenant
  può consentirlo (realtà piccole) con il config modulo
  `consuntivi.approval = {"allow_self_approve": true}`. Default: **false**.
- **ACL di sede.** Elenco, dettaglio, export e sintesi AI applicano l'ACL per sede dell'utente
  (`operator_sites`), come gli incidenti. Il proprietario vede sempre il proprio consuntivo. Gli
  altri servono `timesheets.view_all`; i ruoli ristretti per sede (operatore/lavoratore/reperibile,
  anche con `view_all` da ruolo personalizzato) vedono solo i propri più quelli di ordini nelle loro
  sedi. I ruoli non ristretti (responsabile/auditor/admin) con `view_all` vedono tutto.
- **ACL di sede anche per chi approva (CA-9).** Un approvatore ristretto per sede approva, rimanda
  o valorizza solo consuntivi di ordini nelle sue sedi; un consuntivo senza ordine (quindi senza
  sede) non è approvabile da un ruolo ristretto.
- **Versioning ottimistico.** Colonna `version` su `timesheets`, incrementata a ogni modifica
  (riga, spesa, tariffa, transizione di stato) ed esposta in elenco e dettaglio. Oggi è solo
  esposta: nessuna rotta la richiede in ingresso.
- **Storico completo.** `timesheet_events` registra `create`, `add_line`, `edit_line`,
  `delete_line`, `add_expense`, `edit_expense`, `delete_expense`, `set_rate`, `submit`,
  `request_changes` (con la nota), `approve`. Il dettaglio lo restituisce in `history`.

## Calcolo degli importi

- **Minuti fatturati.** Per ogni riga i minuti si arrotondano per eccesso al multiplo di
  `round_minutes` (config modulo `consuntivi.rates`, default 15).
- **Manodopera.** Arrotondamento al centesimo (half-up) per riga:
  `(minuti_fatturati * tariffa_cent_ora + 30) / 60`. Il CSV usa lo stesso calcolo.
- **Valuta.** La valuta del tenant è `consuntivi.rates.currency` (default `EUR`). Il **totale è
  nella valuta del tenant**; righe e spese in altre valute restano a parte (`total.by_currency`,
  `total.mixed_currency`), mai sommate.
- **Righe senza tariffa.** Non producono importo; il totale segnala `missing_rate`.

## Correzione, valori e ponte dall'intervento (CA-8, CA-14, CA-18 · mig `0139`)

- **Correzione (CA-8).** Un consuntivo in *bozza* o *da correggere* si corregge dal proprietario
  (modifica ed eliminazione di righe e spese; versione +1 e voce nello storico). La nota di chi
  rimanda va in `review_note`; `note` resta per il proprietario e per i materiali dell'intervento.
- **Valori (CA-14).** L'API accetta minuti 1–1440, spese maggiori di zero, valuta di 3 lettere
  maiuscole. Il DB ha vincoli più larghi come rete di sicurezza (minuti 0–1440, tariffe e importi
  non negativi, valuta `^[A-Z]{3}$`), creati `NOT VALID` e validati quando le righe storiche lo
  permettono.
- **Ponte dall'intervento (CA-18, mig `0080`).** `POST /api/orders/:id/timesheet` genera (o riusa)
  una bozza per l'assegnatario, con una riga di manodopera dai minuti lavorati e i materiali nella
  nota. Si genera solo a intervento **completato** (409 altrimenti) e con minuti lavorati (400).
  È idempotente: una sola riga per (ordine, persona) grazie all'indice `timesheet_lines_source_uq`.
  Se i minuti dell'intervento cambiano e il consuntivo è ancora modificabile, la riga si riallinea
  (`line_updated`).

## Tariffe: listino unico per qualifica (CA-6, CA-14 · mig `0156`–`0157`, decisione PO 25/11/2026)

- **Chi consuntiva non sceglie la propria tariffa.** La tariffa indicata vale solo per chi approva
  (`approve`), pianifica (`plan`) o vede i costi (`costs.view`). Per gli altri, e quando non è
  indicata, la tariffa viene dal listino: `app.yw_labor_rate_cents(tenant, persona, giorno)`.
- **Come si sceglie la voce.** Voce di categoria `manodopera`, unità `ora`, attiva e valida nel
  giorno della riga (`valid_from`/`valid_to`, estremi inclusi). Prima le voci di una **qualifica**
  che la persona ha e non è scaduta (la più cara se più d'una), poi la voce per tutti. Per la riga
  dal ponte conta la persona che ha lavorato e il giorno dell'intervento.
- **Senza voce nel listino** la riga resta *da valorizzare* e **chi approva la fissa** durante la
  revisione (`POST /api/timesheets/:id/lines/:lid/rate`).
- **Modificare una riga** non ne cambia la tariffa se chi modifica non può fissarla.
- **Listino.** `app.price_list_items` con prezzo in centesimi (`price_cents`, fonte unica;
  `unit_price` in euro è una colonna calcolata a 2 decimali), `qualification_id`, `valid_from`,
  `valid_to`. API `GET/POST /api/price-items`, `POST/DELETE /api/price-items/:id` (modulo
  `interventi`, modifica con `plan`); in console *Lavoro sul campo → Listino*. La tariffa viene
  copiata sulla riga: se il listino cambia, il consuntivo già scritto non cambia.
- Lo stesso calcolo valorizza le ore di presenza nei costi di commessa (senza voce nel listino, il
  costo orario della commessa). Dettaglio: [proposte-campo-ca4-ca5-ca6](proposte-campo-ca4-ca5-ca6.md).

## Registro unico delle ore (CA-4, mig `0158`, decisione PO 25/11/2026)

- La vista `app.work_hours` (`security_invoker`, isolata per tenant dall'RLS) raccoglie il tempo
  lavorato da quattro fonti, sempre allineata e senza copie:

  | Fonte | Priorità | Fatturabile | Cosa entra |
  |---|---|---|---|
  | intervento | 1 | sì | interventi `completato` con assegnatario e minuti lavorati |
  | ronda | 2 | no | ronde chiuse, escluse quelle chiuse dal sistema |
  | presenza | 3 | no | presenze di cantiere chiuse, escluse quelle chiuse d'ufficio |
  | turno | 4 | no | turni svolti e chiusi |

- **Sovrapposizioni.** Se lo stesso tempo è registrato da più fonti vince la più specifica
  (intervento › ronda › presenza › turno). Una registrazione conta solo per la parte non coperta
  dalle fonti con priorità più alta; la sovrapposizione resta visibile.
- **Ronde e turni** contano come ore di servizio, **non** come righe di consuntivo. Il consuntivo
  nasce dagli interventi (fatturabili); i costi di commessa dalle presenze.
- **API.** `GET /api/work-hours?from&to[&user_id]` (al massimo 62 giorni, default ultimi 7) con
  uno dei moduli `consuntivi`, `workforce`, `cantieri`, `giri`, `interventi`. Restituisce le
  registrazioni (`minutes`, `effective_min`, `overlap_min`, `billable`), i totali per persona e
  giorno e i turni pianificati confrontati con quelli svolti (minuti previsti e svolti, ritardo di
  inizio, turno senza lavoro registrato). Chi approva, pianifica o ha `timesheets.view_all` vede
  tutti; gli altri solo sé stessi.
- **Console.** *Lavoro sul campo → Ore lavorate*.

## Invarianti mantenute

- `work_order_id` resta facoltativo: si consuntiva anche senza ordine.
- Doppia approvazione bloccata (UPDATE condizionale su `state`).
- Dopo l'approvazione il consuntivo è immutabile (nessuna riga, spesa o tariffa).
- Isolamento tenant (RLS) invariato.

## Non ancora fatto

Allegati giustificativi alle spese (oggi c'è solo l'indicatore `has_receipt`), rettifica tracciata
dopo l'approvazione, uso di `version` per bloccare modifiche concorrenti.

## Prove

- `qa/smoke.sh` T13/T14: totale deterministico, submit/approve, doppia approvazione, CSV; la
  tariffa la fissa chi approva.
- `qa/consuntivi-correttezza-e2e.sh` (5/5): arrotondamento half-up (838≠837), versioning,
  self-approval 403, approvazione da terzo, approvato immutabile.
- `qa/consuntivi-bridge-e2e.sh` (6/6): ponte dall'intervento.
- `qa/timesheet-corrections-e2e.sh` (22/22): correzione, valori, tariffa in revisione, ponte.
- `qa/listino-qualifica-e2e.sh` (8/8): tariffe per qualifica e validità.
- `qa/work-hours-e2e.sh` (8/8): registro unico delle ore.
