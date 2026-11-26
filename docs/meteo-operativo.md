# Meteo operativo (fase 4)

Fase 4 del piano «Idealverde su YouWorkforce». Modulo **Meteo operativo** (`meteo`, richiede
Interventi), migrazione `0168`, codice `core/src/weather.rs`, test `qa/weather-e2e.sh`, pagina
**Lavoro sul campo → Meteo**.

**Chi porta le previsioni.** YouWorkforce non chiama servizi meteo (regola «nessuna connessione
verso l'esterno»). Le previsioni le legge Youco (o un altro sistema autorizzato) e le **scrive** in
YW con una chiave di integrazione con il permesso *Pianificare*, come il gestionale scrive clienti e
interventi (vedi [integrazioni.md](integrazioni.md)).

## Zone meteo

Una zona ha un codice (quello che usa chi scrive le previsioni), un nome e una posizione. Ogni sede
usa la zona **fissata** a mano (pagina Meteo → zona → sedi spuntate) oppure, se nessuna è fissata,
la **più vicina** alla sua posizione. Una sede senza posizione e senza zona fissata non riceve avvisi.
Eliminare una zona elimina le sue previsioni; le sedi fissate tornano alla zona più vicina.

## Previsioni

Fasce orarie per zona (di solito 1 o 3 ore, al massimo 24): pioggia in mm, probabilità di pioggia,
vento medio e raffiche in km/h, temperatura, condizione (`sereno`, `poco_nuvoloso`, `nuvoloso`,
`coperto`, `pioggia_debole`, `pioggia`, `temporale`, `neve`, `nebbia`, `vento`), fonte ed emissione.
Una fascia si identifica per zona e inizio: una nuova emissione **sostituisce** la precedente fascia
per fascia. `kind: "osservato"` registra il meteo realmente avvenuto (per lo storico); gli avvisi usano
solo `previsione`.

## Limiti meteo del tipo di attività

Sul tipo di attività (pagina Tipi di attività, con il modulo attivo): pioggia massima (mm nella
fascia dell'intervento), vento massimo (si confronta con il maggiore tra vento e raffiche),
temperatura minima e massima, **ore asciutte dopo** (es. trattamento fitosanitario: niente pioggia
nelle 6 ore successive). Campo vuoto = nessun limite.

## Avvisi sull'intervento

Il controllo dell'intervento (`POST /api/orders/check`, creazione, modifica, Agenda tecnici) aggiunge
avvisi `weather_rain`, `weather_wind`, `weather_cold`, `weather_heat`, `weather_dry_after` quando la
previsione della zona della sede supera i limiti del tipo. Sono **avvisi, mai blocchi**: una previsione
è incerta e la decisione resta al pianificatore. La pioggia di una fascia coperta solo in parte conta
in proporzione (1 ora di una fascia da 4 mm in 3 ore = 1,3 mm). Il messaggio nomina zona, giorno,
orario, valore previsto e limite.

## Interventi a rischio e primo giorno buono

`GET /api/weather/at-risk?days=7`: interventi aperti dei prossimi giorni (non in corso) con un avviso
meteo e, per ciascuno, il **primo giorno buono**: stesso orario, giorni lavorativi, entro 7 giorni,
solo dentro il periodo coperto dalla previsione (oltre non si propone nulla). La pagina Meteo li
elenca con «Sposta» (sposta l'intervento e le prenotazioni dei mezzi, rispettando i vincoli duri con
la conferma «Forza»). L'Agenda tecnici mostra quanti interventi della settimana hanno un avviso meteo.

## API

| Metodo | Percorso | Cosa fa | Permesso |
|---|---|---|---|
| PUT | `/api/integration/weather/zones/{codice}` | crea o aggiorna una zona `{name, lat, lon}` | Pianificare o Impostazioni |
| POST | `/api/integration/weather/slots` | scrive le fasce `{zone, kind?, source?, issued_at?, slots:[{from,to,rain_mm,rain_prob,wind_kmh,gust_kmh,temp_c,condition}]}` (max 2000) | Pianificare o Impostazioni |
| GET | `/api/weather/forecast?days=7` | per zona: giorni (pioggia totale, probabilità massima, vento, minima e massima, condizione prevalente), sedi, ultima emissione e fonte | utente del tenant |
| GET | `/api/weather/at-risk?days=7` | interventi a rischio con gli avvisi e il giorno proposto | Pianificare o Interventi · Vedere tutti |
| POST | `/api/weather/zones/{id}/sites` | fissa le sedi sulla zona `{site_ids}` (sostituisce l'elenco) | Pianificare o Impostazioni |
| DELETE | `/api/weather/zones/{id}` | elimina zona e previsioni | Impostazioni |

I tipi di attività accettano `weather: {rain_max_mm, wind_max_kmh, temp_min_c, temp_max_c,
dry_hours_after}` (se presente sostituisce tutti i limiti; `null` = nessun limite) e lo restituiscono
nell'elenco. Il pianificatore automatico (fase 5) userà gli stessi avvisi per evitare i giorni di
pioggia.
