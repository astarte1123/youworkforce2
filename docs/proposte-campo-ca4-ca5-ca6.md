# Proposte di decisione — Lavoro sul campo (CA-4, CA-5, CA-6)

Stato: **da decidere** (25/11/2026). Sono tre scelte di prodotto che cambiano il modello dei dati:
qui sono preparate come proposte, **non implementate**. Per ognuna: il problema, le opzioni, la
raccomandazione e cosa cambia per chi usa YouWorkforce.

---

## CA-4 · Una sola fonte del tempo lavorato

**Problema.** Oggi lo stesso lavoro può essere registrato in quattro posti che non si parlano:

| Dove | Chi lo scrive | Cosa misura |
|---|---|---|
| minuti dell'intervento (`work_orders.work_minutes`) | il tecnico nel rapporto | tempo sul singolo intervento |
| turni (`shifts`, inizio/fine dall'app) e turni pianificati (`shift_plans`) | il lavoratore / il pianificatore | presenza in servizio |
| presenze di cantiere (`commessa_presenze`, entrata/uscita) | il lavoratore | tempo sulla commessa |
| ronde (`patrol_rounds`, avvio/chiusura) | la guardia | tempo di vigilanza |

Il consuntivo prende solo i minuti dell'intervento (ponte, CA-18). I costi della commessa prendono
solo le presenze. I turni pianificati non sono mai confrontati con quelli svolti. Risultato: ore
contate due volte (intervento dentro una presenza di cantiere) o mai (ronde, turni).

**Opzioni.**
1. **Registro unico delle ore** (nuova tabella «ore lavorate»): ogni fonte scrive una riga con
   persona, inizio/fine, origine (intervento, presenza, ronda, turno) e commessa/ordine. Consuntivi
   e costi leggono solo da lì; le sovrapposizioni si vedono e si risolvono (vince la fonte più
   specifica: intervento > ronda > presenza > turno).
2. **Il consuntivo come unica fonte**: tutte le fonti generano righe di consuntivo (come fa già
   l'intervento). Più semplice, ma il consuntivo diventa anche il dato dei costi di commessa e
   della presenza, che hanno tempi e approvazioni diversi.
3. **Lasciare separato** e aggiungere solo un report di riconciliazione (differenze tra fonti).

**Raccomandazione: opzione 1.** È l'unica che evita doppi conteggi senza forzare un'approvazione
dove non serve. Si introduce senza rompere nulla: le fonti attuali continuano a scrivere e in più
alimentano il registro; poi consuntivi e costi passano a leggere dal registro.

**Cosa cambia per gli utenti.** Il responsabile vede per persona e giorno le ore con l'origine e
le sovrapposizioni segnalate; il turno pianificato si confronta con quello svolto (ritardi,
assenze). Stima: 1 migrazione, ~3 slice (registro + scrittura dalle fonti; consuntivo e costi dal
registro; riconciliazione turni pianificati/svolti).

**Da decidere.** (a) Regola di precedenza in caso di sovrapposizione; (b) se le ore di ronda vanno
nel consuntivo o solo nel report di vigilanza; (c) se il turno svolto senza intervento genera ore
fatturabili.

---

## CA-5 · Squadre e gruppi di persone: un solo concetto

> **FATTO (decisione PO 25/11/2026, opzione 1, mig `0155`).** Ogni squadra è diventata un gruppo di
> persone di tipo «squadra» con lo stesso id (nome con « (squadra)» se esisteva già un gruppo omonimo:
> i membri non si uniscono mai a un gruppo che dà permessi). `commessa_teams` → `commessa_groups`
> (`group_id`), tabelle `teams`/`team_members` eliminate, controllo «stesso tenant» rigenerato.
> `/api/teams` resta come alias dei gruppi (leggibile da chi pianifica, per assegnare un gruppo a una
> commessa); in *Utenti e sedi* la sezione Squadre rimanda a *Utenti e ruoli → Gruppi*. Le presenze di
> cantiere verificano l'appartenenza al gruppo assegnato.

**Problema.** Esistono due modi per raggruppare persone:
- **squadre** (`teams`, modulo *Forza lavoro e turni*): usate dalle commesse di cantiere;
- **gruppi di persone** (`user_groups`, nel nucleo): usati da ordini, ronde, permessi, escalation.

Le squadre richiedono il modulo Forza lavoro: un'azienda con Cantieri ma senza Forza lavoro non può
assegnare squadre alle commesse (403). Chi gestisce le persone le deve inserire due volte.

**Opzioni.**
1. **Un solo concetto: i gruppi di persone.** Le squadre diventano gruppi (con un'etichetta
   «squadra» se serve distinguerle in elenco). Migrazione dati: ogni squadra → un gruppo con gli
   stessi membri; le commesse puntano al gruppo. Le API squadre restano come alias per un periodo.
2. **Tenere entrambi ma dichiarare la dipendenza**: Cantieri richiede Forza lavoro (manifest);
   niente 403 inattesi, ma resta il doppio inserimento.
3. **Squadre nel nucleo** (fuori da Forza lavoro), separate dai gruppi.

**Raccomandazione: opzione 1.** I gruppi hanno già tutto (ACL su ordini/ronde, ruoli
personalizzati, escalation). Una sola anagrafica, nessuna dipendenza nascosta tra moduli.

**Cosa cambia per gli utenti.** In *Utenti e gruppi* le squadre compaiono come gruppi; le commesse
si assegnano a un gruppo; Cantieri funziona anche senza Forza lavoro. Stima: 1 migrazione con
copia dati + adeguamento API/console commesse; e2e su commesse e squadre.

**Da decidere.** (a) Se serve distinguere visivamente «squadra operativa» da «gruppo di permessi»;
(b) per quanto tempo tenere le vecchie API squadre.

---

## CA-6 · Un listino unico, in centesimi

> **FATTO (decisione PO 25/11/2026, opzione 1, mig `0156`–`0157`).** Il listino ha il prezzo in
> **centesimi** (`price_cents`, fonte unica; `unit_price` in euro è calcolato, 2 decimali esatti), la
> **qualifica** (`qualification_id`, solo per la manodopera a ore) e la **validità** (`valid_from`,
> `valid_to`). Le righe dell'intervento copiano il prezzo in centesimi (`unit_price_cents`).
> La tariffa di manodopera di una persona in un giorno la calcola `app.yw_labor_rate_cents`:
> voce «manodopera» a ore attiva e valida quel giorno, prima quella di una qualifica che la persona
> ha (non scaduta, la più alta se più d'una), poi quella per tutti. La usano il consuntivo (riga
> manuale, ponte dall'intervento, agente AI) e i **costi di commessa** (ogni presenza valorizzata con
> la tariffa della persona; senza voce nel listino, il costo orario della commessa). Pagina Listino:
> qualifica e validità nel modulo. e2e `qa/listino-qualifica-e2e.sh` 8/8.

**Problema.** Ci sono più valorizzazioni che non si parlano:
- listino (`price_list_items.unit_price`, euro con decimali, letto come numero in virgola mobile);
- righe dell'intervento (`work_order_lines.unit_price`, euro);
- consuntivo (`timesheet_lines.rate_cents`, centesimi; dal 25/11 la tariffa di manodopera viene
  dalla prima voce «manodopera a ore» del listino, CA-14);
- costo orario della commessa (`commesse.labor_cost_hour`, euro).

Una tariffa cambiata nel listino non si riflette ovunque, e i calcoli in virgola mobile possono
dare differenze di un centesimo sui totali.

**Opzioni.**
1. **Listino unico in centesimi** con voci per *ruolo/qualifica* (es. «tecnico», «capocantiere»)
   e validità nel tempo (dal/al). Interventi, consuntivi e commesse leggono il prezzo da lì e ne
   salvano una copia sulla riga (il prezzo storico non cambia se il listino cambia).
2. **Solo conversione in centesimi** dei campi esistenti, senza unificare.
3. **Listino per cliente** oltre a quello generale (prezzi concordati).

**Raccomandazione: opzione 1**, con il listino per cliente (3) come estensione successiva.

**Cosa cambia per gli utenti.** Una sola pagina *Listino* con le tariffe orarie per qualifica e i
materiali; il tecnico non inserisce prezzi; il responsabile vede da quale voce viene ogni prezzo.
Stima: 1 migrazione (centesimi + qualifica + validità, conversione dei dati esistenti), adeguamento
di interventi/consuntivi/commesse, e2e sui totali al centesimo.

**Da decidere.** (a) Tariffe per qualifica o per persona; (b) se la commessa ha un costo orario
proprio o usa il listino; (c) arrotondamenti (oggi: minuti a 15, importi half-up al centesimo).

---

### Collegamenti
- Registro errori: CA-4, CA-5, CA-6 (analisi).
- Già fatto e collegato: CA-14 (tariffa dal listino, valute), CA-18 (ponte a intervento
  completato), CA-8 (correzione consuntivi), CA-11 (chi vede i costi).
