# Matrice moduli e dipendenze (Contratto comune)

| Codice | Nome | Tipo | Stato build | Dipendenze capacità |
|---|---|---|---|---|
| `loneworker` | Lone Worker e centrale allarmi | prodotto | **implementato** | sorgente evento (badge/PWA) + `goalert` |
| `goalert` | GoAlert escalation | connettore | **SIMULATO** (reale via profilo) | — (prerequisito dei percorsi di escalation) |
| `traccar` | Traccar badge | connettore | contratto + sim | ingest token |
| `iot` | Dispositivi e IoT | prodotto | ingest HTTP + sim | — (può lavorare senza loneworker) |
| `thingsboard` | ThingsBoard industriale | connettore | **off / esterno** (BUSL) | iot |
| `blues` | Blues sperimentale | connettore | **off** (replay) | — |
| `workforce` | Forza lavoro e turni | prodotto | parziale (assegnazione manuale) | — |
| `interventi` | Interventi e cantieri | prodotto | **implementato** (ordini) | assegnazione manuale |
| `consuntivi` | Ore, spese, consuntivi | prodotto | **implementato** (righe+spese+CSV) | fonte dati dichiarata; **no AI** |
| `assistant` | YouWorker.ai + diario vocale | prodotto | backlog (Prompt 7/8) | provider YouWorker.ai contrattato |
| `qualifiche`,`prevenzione` | — | prodotto | `available=false` ("non disponibile in questa release") | — |

## Regole invarianti
- Ogni **modulo funzionale** è attivabile/disattivabile per tenant dal superadmin, **senza deploy**.
- Moduli spenti: UI, API, job e nuovi ingressi rispettano la stessa decisione; lo storico resta leggibile ai ruoli autorizzati.
- Sempre obbligatori: identità, isolamento, permessi, configurazione tenant, audit.
- Stati UI: `non_abilitato · da_configurare · attivo · degradato · in_disattivazione`.
- Disattivare `loneworker` con incidenti aperti → **bloccato** (piano di disattivazione, vedi `set_module`).
- GoAlert è l'**unico** motore di escalation. Nessun secondo motore.
- Spegnere AI/dettatura non toglie inserimento manuale, check-in, SOS.

## Profili collaudabili sullo stesso build
- **Workforce**: workforce+interventi+consuntivi, no IoT/AI.
- **Sicurezza**: loneworker+goalert+(traccar), no pianificazione. ← dimostrato in questa build
- **IoT tecnico**: iot(+thingsboard), goalert se richiesto.
- **Integrato**: tutti.
