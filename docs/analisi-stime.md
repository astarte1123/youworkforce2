# Analisi e stime (fase 3)

Fase 3 del piano «Idealverde su YouWorkforce». Modulo **Analisi e stime** (`analisi`, richiede
Interventi), migrazione `0167`, codice `core/src/analytics.rs`, test `qa/analytics-e2e.sh`, pagina
**Lavoro sul campo → Analisi**. Tutto è calcolato dai dati di YW con regole dichiarate: nessun
modello opaco, ogni stima dice su cosa si basa.

## Origine ed extra degli interventi

Ogni intervento ha un'**origine** (`origin`): `manuale` (console), `piano` (piano di manutenzione),
`richiesta` (convertito da una richiesta), `integrazione` (dal gestionale). E un indicatore
**extra** (`is_extra`): lavoro fuori contratto o programma. È vero per gli interventi nati da una
richiesta; in console si spunta «Lavoro extra»; dal gestionale arriva con `"extra": true` (CSV:
colonna `extra`). La migrazione ricava l'origine dello storico con regole del prodotto (codice
esterno, richiesta collegata, piano che l'ha generato).

## Stima dallo storico

Per un nuovo intervento con tipo di attività, nell'ordine:

1. **stessa sede**: mediana dei minuti lavorati negli interventi chiusi dello stesso tipo nella sede
   (almeno 3);
2. **stessa squadra**: stesso tipo, stessa squadra (almeno 5);
3. **tipo**: tutte le sedi (almeno 5);
4. altrimenti la **durata standard** del tipo di attività (base + minuti per unità × quantità).

Con la quantità (m², metri, piante…) la mediana si scala sul rapporto con la quantità tipica degli
interventi usati, tra 0,5× e 2×. La risposta riporta anche l'80° percentile («fino a … nell'80% dei
casi»), la durata standard e la spiegazione («12 interventi di questo tipo in questa sede»). Il
controllo dell'intervento (`POST /api/orders/check`) la propone in `history_estimate` e la console
mostra «📊 Dallo storico … usa questa durata».

**Precisione.** Per ogni intervento chiuso del periodo si calcola la stima con i soli interventi
precedenti e la si confronta con il tempo reale e con la durata standard: errore medio in minuti ed
entro ±20%, in totale e per tipo. È la verifica della fase («la stima sbaglia meno della durata
standard»).

## Altre analisi

- **Durate reali** per tipo, per tipo e sede o per tipo e squadra: interventi, media, mediana, 80°
  percentile, durata standard, scostamento.
- **Lavori extra**: ultimi N mesi, per mese (extra, totale, minuti, richieste arrivate), clienti e
  sedi che ne generano di più.
- **Ricorrenze**: per ogni piano di manutenzione attivo con sede e tipo, frequenza a contratto contro
  intervallo medio reale tra gli interventi chiusi (ultimo anno): in linea, più frequente o meno
  frequente del contratto (±25%), in ritardo (ultimo intervento oltre 1,5 volte la frequenza),
  nessun intervento svolto.
- **Piano contro realtà** per settimana: in programma, chiusi, annullati, scaduti aperti, minuti
  pianificati e lavorati, percentuale avviata il giorno previsto.

## API

| Metodo | Percorso | Cosa restituisce |
|---|---|---|
| GET | `/api/analytics/estimate?activity_type_id&site_id&group_id&quantity` | stima dallo storico con base, numero di interventi, 80° percentile, standard |
| GET | `/api/analytics/durations?by=type\|site\|team&from&to` | durate reali (default ultimo anno) |
| GET | `/api/analytics/accuracy?days=90` | precisione della stima contro la durata standard |
| GET | `/api/analytics/extras?months=12` | extra per mese, clienti e sedi |
| GET | `/api/analytics/recurrences` | ricorrenze dei piani |
| GET | `/api/analytics/plan-vs-actual?weeks=8` | piano contro realtà per settimana |

Permessi: «Pianificare», «Interventi · Vedere tutti» o «Costi · Vedere». `POST /api/orders` e
`/api/orders/:id/edit` accettano `is_extra`; l'elenco degli interventi restituisce `origin` e
`is_extra`. Le stesse analisi servono al pianificatore automatico (fase 5) per le durate.
