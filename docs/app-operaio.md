# App dell'operaio per il lavoro sul campo

L'app del lavoratore (PWA in `console/static/worker/`, installabile su Android e iPhone) diventa
lo strumento di chi va in campo. Deve dire cosa fare, in che ordine, dove, con cosa e con che
tempo. Vale per Idealverde e per qualsiasi tenant con lavori simili (manutenzione del verde,
impianti, pulizie): le sezioni compaiono secondo i moduli attivi.

## Piano

| Fase | Contenuto | Stato |
|---|---|---|
| M1 | «La mia giornata»: interventi propri e della squadra in ordine, spostamenti, squadra e assenti, mezzi da ritirare, meteo per sede e fascia con gli avvisi, percorso della giornata in Maps, prossima giornata, copia senza rete | **fatta (26/11/2026)** |
| M2 | Scheda intervento completa; «Segnala problema / lavoro extra» con foto (diventa una richiesta per il pianificatore); foto prima e dopo | da fare |
| M3 | Cambi di programma: avviso in app (badge «cambiato») e poi notifiche push dal nostro server | da fare |
| M4 | Caposquadra (avvia, chiude e segna le presenze per la squadra) e uso offline completo con coda delle azioni | da fare |

Restano da decidere con il PO:

- chi chiude gli interventi: il caposquadra per tutti o ognuno i suoi;
- che telefoni si usano: aziendali, uno per squadra, oppure personali;
- quali lingue oltre a italiano e inglese;
- un'app da store, solo se servono funzioni che il browser non dà.

## M1 — La mia giornata

`GET /api/my/day?date=YYYY-MM-DD` oppure `?next=1` (prossima giornata con interventi, entro 14
giorni). Data ammessa: da 14 giorni prima a 14 giorni dopo oggi. Codice `core/src/myday.rs`, test
`qa/myday-e2e.sh`.

- **Interventi:** quelli assegnati all'utente o a un suo gruppo, nel giorno (ora del tenant),
  ordinati per orario. Esclusi gli annullati. Per ognuno:
  - orario, durata, tipo e quantità;
  - sede con indirizzo, note e finestre di accesso, cliente, istruzioni, stato;
  - mezzi prenotati (modulo Mezzi);
  - meteo della sede nella fascia e avvisi del tipo di attività (modulo Meteo).
- **Spostamenti:** stesso modello della pianificazione automatica, cioè linea d'aria × fattore /
  velocità + sistemazione. Si parte dal deposito della squadra, altrimenti da quello del tenant; si
  passa per le tappe e si conta il rientro. Il totale comprende il rientro.
- **Squadra:** membri dei gruppi «squadra» dell'utente, con chi è assente quel giorno.
- **Mezzi da ritirare:** tutti quelli prenotati sugli interventi della giornata, una volta sola.
- **Meteo:** riepilogo della giornata per zona delle sedi visitate.
- **Percorso della giornata:** link di Google Maps deposito → tappe → deposito.

Nell'app, in alto, le schede **Oggi** e **Prossima giornata**. La giornata scaricata resta in
memoria sul telefono e si vede anche senza rete, con l'avviso. Ogni operaio vede solo la propria
giornata e quella delle proprie squadre.

**Verifica su Idealverde:** Simone Gallo (Squadra Nord), lunedì 30/11.

- 3 interventi in ordine, 8 h di lavoro, 1 h 31 min di spostamento (28,6 km).
- Meteo di Milano Centro e Nord e Sud-Ovest; squadra di 8; Furgone FV 101 da ritirare al deposito;
  note di accesso.
- L'app mostra il problema del piano attuale: l'intervento 3 inizia prima che finisca il 2.
