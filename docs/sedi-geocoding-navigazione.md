# Sedi: geocodifica indirizzo + navigazione dal campo

Nel form **Sede** (Utenti e sedi → Sedi) l'indirizzo si trasforma in posizione sulla mappa:

- Pulsante **«Trova sulla mappa dall'indirizzo»**: geocodifica l'indirizzo e compila
  automaticamente **latitudine/longitudine**.
- **Mappa con segnaposto trascinabile**: si corregge la posizione esatta (cancello, ingresso
  cantiere) trascinando il pin o cliccando sulla mappa; lat/lon si aggiornano in tempo reale.
- I campi lat/lon restano modificabili a mano e sincronizzano il segnaposto.

## Self-contained: la geocodifica è lato BROWSER

Per rispettare il principio «il server YW non chiama servizi esterni», la geocodifica avviene
**nel browser dell'operatore** (fetch diretto a Nominatim/OpenStreetMap), non dal core. Il server
riceve e salva solo lat/lon già risolti. Le tile della mappa sono le stesse OSM già usate altrove.
Se la geocodifica non è disponibile (offline/blocco), si inserisce la posizione a mano o col
segnaposto: nessuna dipendenza dura.

**Anche l'indirizzo di un dispositivo (TA-15, decisione PO 25/11/2026).** Prima la scheda di un
dispositivo chiedeva l'indirizzo dell'ultima posizione al server, che chiamava Nominatim e
teneva una cache (`app.geocode_cache`): le posizioni dei lavoratori uscivano dal server. Ora la
scheda mostra **«Mostra indirizzo»**: solo se l'operatore lo preme, il **browser** lo chiede a
OpenStreetMap. L'endpoint `/api/geocode/reverse` è stato tolto e la cache eliminata
(mig `0151`). e2e `qa/geocode-e2e.sh`.

## Navigazione dalla PWA del lavoratore

Nella PWA «Oggi», ogni intervento con posizione mostra **🧭 Naviga**: apre l'app di navigazione
del telefono (link universale Google Maps) verso le **coordinate** della sede (se presenti, più
precise) o verso l'**indirizzo**. Così il lavoratore raggiunge il cantiere o il luogo dell'intervento
senza copiare indirizzi a mano.

Le sedi sono condivise da Interventi e Cantieri: la stessa posizione serve per la navigazione verso
l'appuntamento (assistenza) e verso il cantiere.
