# Matrice moduli e dipendenze (Contratto comune)

| Codice | Nome | Tipo | Stato build | Dipendenze capacità |
|---|---|---|---|---|
| `loneworker` | Lone Worker e centrale allarmi | prodotto | **implementato** | sorgente evento (badge/PWA) + `goalert` |
| `goalert` | GoAlert escalation | connettore | **UPSTREAM REALE** (v0.35.0, EP 2 livelli, **consegna webhook verificata** via `qa/goalert-delivery.sh`); SIMULATO se `GOALERT_BASE` off | — (prerequisito dei percorsi di escalation) |
| `traccar` | Traccar badge | connettore | **UPSTREAM REALE** (v6.17.0, event forwarding → `/api/ingress/traccar`; alarm sos→sos, fallDown→man_down) | ingest token |
| `iot` | Dispositivi e IoT | prodotto | ingest HTTP + **ThingsBoard reale** | — (può lavorare senza loneworker) |
| `thingsboard` | ThingsBoard industriale | connettore | **UPSTREAM REALE** (v3.8.1, Rule Engine → `/api/ingress/thingsboard`; soglia → `sensore_critico`); BUSL, istanza esterna | iot |
| `workforce` | Forza lavoro e turni | prodotto | parziale (assegnazione manuale) | — |
| `interventi` | Interventi e cantieri | prodotto | **implementato** (ordini) | assegnazione manuale |
| `consuntivi` | Ore, spese, consuntivi | prodotto | **implementato** (righe+spese+CSV) | fonte dati dichiarata; **no AI** |
| `assistant` | youworker.ai (AI di YouCo) in ogni processo + diario vocale | prodotto | **REALE**: connessione youworker.ai (enc:v1) da superadmin, feature per-processo da tenant_admin; assistente con grounding deterministico, sintesi incidenti, suggerimento regole sensori, anomalie check-in, assist interventi/consuntivi, diario vocale STT server; fallback deterministico se non configurato | provider OpenAI-compatibile intercambiabile; azioni critiche sempre manuali |
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
