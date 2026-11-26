# Pianificazione automatica (fase 5)

Fase 5 del piano «Idealverde su YouWorkforce». Modulo **Pianificazione automatica** (`pianificazione`,
richiede Interventi; usa Qualifiche, Mezzi, Meteo e Analisi se attivi), migrazione `0170`, codice
`core/src/planning.rs`, test `qa/planning-e2e.sh`, pagina **Pianificazione → Automatica**.

Decisioni PO (piano, decisioni 1 e 3):

- YW espone il problema con un **contratto dati** (`yw-planning/1`), **valida ogni proposta** con gli
  stessi vincoli della pianificazione a mano e ha un **motore di base nativo**: il cliente ha sempre un
  piano valido. Un motore esterno (Youco) può proporre sullo stesso contratto.
- **Il motore propone, la persona decide**: la proposta è una bozza da rivedere e applicare.
- Tempi di spostamento: distanza in linea d'aria × fattore di percorso / velocità media + minuti di
  sistemazione. Nessun servizio esterno.

## Impostazioni

Pagina Automatica → **Impostazioni** (permesso *Pianificare* o *Impostazioni*):

- **deposito** da cui partono e dove tornano le squadre, anche diverso per squadra;
- inizio e fine giornata, giorni lavorativi, **straordinario ammesso** (minuti oltre la fine giornata);
- **velocità media** (25 km/h), **fattore di percorso** (1,3), **minuti di sistemazione** (5);
- **meteo sfavorevole**: evitalo oppure ignoralo.

Senza deposito gli spostamenti del primo e dell'ultimo intervento non si stimano: la pagina lo segnala.

## Motore di base (`yw-base-1`)

Deterministico e spiegabile:

1. **Interventi da pianificare.** Gli interventi aperti del periodo senza data o senza squadra.
   Con «ripianifica» anche quelli già assegnati. Restano dove sono quelli **fissati** dal pianificatore
   (📌 nel dettaglio dell'intervento), quelli avviati e quelli già passati. Le squadre sono i gruppi
   di tipo «squadra».
2. **Ordine.** Priorità alta prima, poi data voluta, poi durata maggiore. La durata è quella
   stimata dell'intervento, altrimenti la durata standard del tipo di attività.
3. **Inserimento.** Per ogni squadra idonea, ogni giorno lavorativo e ogni posizione nella giornata
   gli orari si ricalcolano dal deposito, con gli spostamenti. Una squadra è idonea se ha abbastanza
   persone presenti (assenze escluse) e se qualcuno ha i patentini validi quel giorno, anche quelli
   richiesti dai mezzi. I vincoli:
   - **finestre orarie della sede**;
   - fine giornata più lo straordinario ammesso;
   - **mezzi del tipo richiesto liberi**, cioè non prenotati da interventi esterni alla proposta né da
     altre voci della proposta;
   - con il meteo da evitare: se all'ora più presto la previsione supera i limiti del tipo di attività,
     si prova più tardi, a passi di 30 minuti.

   Si sceglie il costo minore. Il costo somma:
   - i minuti di spostamento in più;
   - lo straordinario (×2);
   - 45 per giorno di distanza dalla data voluta;
   - 20 per il cambio di squadra;
   - 600 per il meteo sfavorevole.
4. **Miglioramento.** Ogni intervento si toglie e si reinserisce dove costa meno, finché il costo
   totale scende (al massimo 6 giri o 20 secondi). Gli interventi rimasti fuori riprovano dopo ogni giro.
5. **Non pianificati.** Ciascuno ha il motivo più frequente tra i tentativi: sede senza posizione,
   nessuna squadra con il patentino valido, nessuna squadra con abbastanza persone, nessun mezzo
   libero, non entra negli orari della sede o nelle giornate.

## Validazione della proposta

Ogni voce passa il controllo dell'intervento della fase 1 (orario della sede, patentini, assenze,
mezzo fermo, meteo). Si aggiungono le sovrapposizioni **dentro la proposta** (stessa squadra o stesso
mezzo) e i mezzi già prenotati da interventi esterni alla proposta. Non si usano i controlli
«sovrapposizione» e «mezzo prenotato» basati sugli orari attuali: guarderebbero gli interventi che la
proposta stessa sposta.

Il riepilogo riporta:

- interventi, pianificati, non pianificati e cambiati;
- vincoli rigidi violati e avvisi;
- minuti e chilometri di spostamento e straordinario, confrontati con il **piano attuale** degli stessi
  giorni (stesso modello di spostamento).

## Applicazione

`Applica i selezionati` ricontrolla ogni voce adesso:

- un intervento **modificato dopo la proposta** (versione diversa) non si applica;
- un intervento **non più da pianificare** (avviato, chiuso o annullato) non si applica;
- un intervento con un **vincolo rigido** non si applica, salvo `force`.

Le prenotazioni dei mezzi di tutte le voci si rifanno insieme, in una transazione: i mezzi si
scambiano di orario tra le voci. Un conflitto vero lo ferma il vincolo del database e i mezzi di quella
voce tornano come prima. Il motivo di ogni voce non applicata compare accanto all'intervento. La
persona assegnata resta solo se fa parte della nuova squadra. Una bozza si può scartare; una proposta
applicata no.

## Motore esterno (Youco)

1. `GET /api/planning/problem?from&to[&replan]` dà il problema nel contratto `yw-planning/1`:
   - impostazioni e modello di spostamento, giorni, squadre con depositi e membri;
   - persone con patentini (e scadenza) e assenze;
   - interventi con posizione, finestre orarie, durata, persone e patentini richiesti, tipi di mezzo,
     priorità, data voluta, squadra attuale, se fisso, **giorni di meteo sfavorevole**;
   - mezzi con le prenotazioni esterne.
2. `POST /api/planning/runs/external` consegna la proposta:
   `{from, to, engine, items:[{order_id, group_id | user_id, start, end, asset_ids?, seq?} | {order_id, reason}]}`.
   YW la valida e la tiene come bozza, uguale a quella del motore di base.

Si usa con una chiave di integrazione con il permesso *Pianificare* (vedi
[integrazioni.md](integrazioni.md)).

## Prova sullo storico

`POST /api/planning/backtest {from, to}` ripianifica gli interventi **completati** di un periodo
passato come se fosse il suo primo giorno e confronta con il piano fatto a mano: vincoli rigidi di
entrambi, spostamenti, straordinario, motivi dei non pianificati. Non salva niente.

**Verifica della fase (Idealverde, dati fittizi, 4 settimane dal 26/10 al 20/11/2026, 333 interventi).**

| | Motore di base | Piano a mano |
|---|---|---|
| Vincoli rigidi violati | 0 | 35 |
| Spostamenti | 4.620 km | 5.669 km (−18,5%) |
| Straordinario | 249 min | 3.356 min |
| Non pianificati | 19 | 0, ma con 35 violazioni |

I 19 non pianificati: patentino PLE o trattore non valido in nessuna squadra, orari della sede troppo
stretti, furgoni esauriti. Il piano a mano li aveva messi comunque, violando i vincoli. La verifica va
ripetuta sullo storico vero.

## API

| Metodo | Percorso | Cosa fa | Permesso |
|---|---|---|---|
| GET/PUT | `/api/planning/settings` | deposito, giornata, giorni, spostamenti, straordinario, meteo, depositi per squadra | Pianificare o Impostazioni |
| POST | `/api/planning/runs` | motore di base: `{from, to, replan?, group_ids?}` → bozza con voci e riepilogo | Pianificare |
| GET | `/api/planning/runs` · `/api/planning/runs/{id}` | elenco e dettaglio delle proposte | Pianificare o Interventi · Vedere tutti |
| POST | `/api/planning/runs/{id}/apply` | applica `{order_ids?, force?}` | Pianificare |
| DELETE | `/api/planning/runs/{id}` | scarta la bozza | Pianificare |
| GET | `/api/planning/problem` | contratto dati `yw-planning/1` | Pianificare |
| POST | `/api/planning/runs/external` | proposta di un motore esterno | Pianificare |
| POST | `/api/planning/backtest` | prova sullo storico (sola lettura) | Pianificare o Interventi · Vedere tutti |
| POST | `/api/orders/{id}/plan-lock` | fissa o sblocca l'intervento `{locked}` | Pianificare |

Il periodo è di massimo 31 giorni. L'elenco degli interventi restituisce `plan_locked`.

**Limiti noti.** Gli spostamenti sono in linea d'aria × fattore: si potranno sostituire con tempi
misurati dall'app del lavoratore. Il motore pianifica per squadra, non per singola persona. I turni
delle persone non entrano: valgono la giornata delle impostazioni e le assenze.
