# Lavoro sul campo — Fase 1: attività, mezzi, qualifiche e vincoli

Prima fase del piano «Idealverde su YouWorkforce» (primo caso reale: 60–70 giardinieri in squadre,
26/11/2026). Mette in YW le fondamenta per pianificare con i vincoli, a mano o con un pianificatore
automatico: **YW resta l'unico arbitro dei vincoli rigidi**, qualunque sia lo strumento che propone
il piano. Migrazione `0165`, codice in `core/src/constraints.rs`, test `qa/field-constraints-e2e.sh`.

## Tipi di attività

Catalogo per azienda (modulo Interventi, permesso `plan`): cosa si fa, quanto dura, cosa serve.

| Campo | Significato |
|---|---|
| `name` | nome unico nell'azienda (es. «Taglio erba e rifilature») |
| `unit` | unità della quantità: `intervento`, `m2`, `ml` (metri lineari), `n` (numero, es. piante), `ore` |
| `base_minutes` + `minutes_per_unit` | durata stimata = base + minuti per unità × quantità |
| `crew_size` | persone necessarie (avviso se la squadra ne ha meno disponibili) |
| qualifiche richieste | almeno una persona disponibile le deve avere valide alla data |
| mezzi richiesti | per tipo (`kind` del mezzo, es. `piattaforma` × 1): avviso se non prenotati |

L'intervento ha `activity_type_id`, `quantity`, `estimated_minutes`; senza fine indicata la fine è
inizio + stima. Anche i piani di manutenzione hanno tipo e quantità, e l'intervento generato li
eredita con la durata. Un tipo usato da interventi o piani non si elimina (si disattiva). L'elenco
mostra accanto alla durata standard la **durata reale media** degli interventi completati.

## Finestre orarie delle sedi

`sites.access_windows` = `[{"days":"1111100","from":"08:00","to":"17:00"}, …]`, ora locale della
sede (o dell'azienda); vuoto = sempre. Un intervento fuori finestra è un vincolo duro.

## Mezzi e attrezzature (modulo `mezzi`)

- **Stato**: `disponibile`, `fermo`, `dismesso`. Un mezzo non disponibile non si prenota (salvo «Forza»).
- **Qualifica per l'uso** (`required_qualification_id`): se il mezzo è prenotato su un intervento,
  qualcuno della squadra la deve avere valida.
- **Scadenze** (`asset_deadlines`): revisione, tagliando, assicurazione, verifica, altro; una scadenza
  superata è un avviso sull'intervento e compare nello scadenziario.
- **Prenotazioni** (`asset_bookings`): per intervento (nell'orario dell'intervento), squadra o persona.
  Il database impedisce due prenotazioni sovrapposte dello stesso mezzo (vincolo di esclusione con
  `btree_gist`). Le prenotazioni di un intervento seguono i suoi spostamenti; senza orario si liberano.

## Controllo dei vincoli

`check_order` valuta l'intervento come sarebbe dopo la modifica. Le violazioni seguono il contratto di
`scheduling.rs`: **dure** = 409 «Conflitto: …» salvo intestazione `x-force: 1` («Forza»), **avvisi** =
restituiti in `warnings`.

| Codice | Tipo | Quando |
|---|---|---|
| `site_window` | duro | fuori dalle finestre orarie della sede |
| `qualification` | duro | nessuna persona disponibile ha una qualifica richiesta dal tipo di attività, valida alla data |
| `asset_qualification` | duro | nessuna persona disponibile ha la qualifica richiesta da un mezzo prenotato |
| `absence` | duro | la persona assegnata è assente |
| `asset_down` | duro | mezzo fermo o dismesso |
| `asset_busy` | duro | mezzo già prenotato in quel periodo |
| `absence_members` | avviso | membri della squadra assenti |
| `crew` | avviso | persone disponibili meno di quelle richieste dal tipo |
| `asset_missing` | avviso | mezzi richiesti dal tipo non prenotati |
| `asset_deadline` | avviso | scadenza del mezzo superata |
| `overlap` | avviso | la stessa persona o squadra ha un altro intervento sovrapposto |

Il controllo si applica a creazione, modifica, assegnazione a persona e a squadra. Le qualifiche
contano solo con il modulo Qualifiche attivo, i mezzi solo con il modulo Mezzi.

## API

| Metodo | Percorso | Cosa fa |
|---|---|---|
| GET/POST | `/api/activity-types` | elenco (con unità disponibili e durata reale media) · nuovo tipo |
| POST/DELETE | `/api/activity-types/:id` | modifica · elimina (409 se in uso) |
| POST | `/api/orders/check` | anteprima dei vincoli senza salvare, con durata stimata e fine calcolata |
| GET | `/api/orders/checks?from&to` | vincoli degli interventi aperti del periodo (max 31 giorni), per l'agenda |
| POST | `/api/orders`, `/api/orders/:id/edit` | accettano `activity_type_id`, `quantity`, `asset_ids` (e `clear_activity`) |
| GET/POST | `/api/asset-bookings` | prenotazioni del periodo (max 93 giorni) · nuova prenotazione |
| DELETE | `/api/asset-bookings/:id` | annulla la prenotazione |
| GET/POST | `/api/assets/:id/deadlines` | scadenze del mezzo · nuova scadenza |
| POST/DELETE | `/api/asset-deadlines/:id` | segna fatta (`{"done":true}`) · elimina |
| GET | `/api/deadlines?days=60` | scadenziario: qualifiche delle persone e scadenze dei mezzi entro N giorni, anche già scadute |

Le sedi accettano `access_windows`; i mezzi `status` e `required_qualification_id` (`""` = nessuna).
La dashboard ha il riquadro `deadlines` (Scadenze) con il modulo Qualifiche o Mezzi.

Un pianificatore esterno (fase 5) propone interventi con squadra, mezzi e orari usando gli stessi
endpoint: le proposte che violano un vincolo duro vengono rifiutate con il motivo.
