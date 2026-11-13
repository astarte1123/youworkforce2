# Ronde e punzonature — Campo Slice 2 (modulo «giri»)

Giri di controllo per guardie/manutentori: un **giro** (route) è una sequenza ordinata di
**checkpoint**; l'operatore esegue una **ronda** (round) e **punzona** (punch) ogni checkpoint; a
fine giro si calcola la **compliance**. Modulo **`giri`** indipendente: funziona senza
interventi/consuntivi/workforce/loneworker, e viceversa. Additivo e non distruttivo.

## Tipi di checkpoint

| Tipo | Verifica punzonatura |
|---|---|
| `qr` | il codice scansionato combacia con `code` atteso |
| `nfc` | idem (payload tag NFC) |
| `beacon` | idem (id beacon) |
| `gps` | la posizione dell'operatore è entro `radius_m` dal punto (`lat`/`lon`), via haversine |

## Modello dati (mig `0075_patrols.sql`)

- `patrol_routes(id, tenant_id, name, site_id?, window_minutes?, active, created_at)` —
  UNIQUE(tenant_id,name). `window_minutes` = finestra max per completare il giro.
- `patrol_checkpoints(id, tenant_id, route_id, name, position, kind, code?, lat?, lon?, radius_m, geofence_id?)`.
- `patrol_rounds(id, tenant_id, route_id, user_id, started_at, ended_at?, status)` —
  status `in_corso|completato|incompleto`.
- `patrol_punches(id, tenant_id, round_id, checkpoint_id, user_id, punched_at, method, lat?, lon?, ok, note)` —
  UNIQUE(round_id,checkpoint_id) (una punzonatura per checkpoint, l'ultima vince).

Tutte RLS `tenant_isolation` + GRANT app_rt; FK CASCADE. Il modulo è registrato in
`modules_catalog` (code `giri`).

## API

Configurazione (richiede modulo `giri` + capability `plan`/is_planner):

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/patrol-routes` | giri + checkpoint |
| POST | `/api/patrol-routes` | crea giro `{name, site_id?, window_minutes?}` |
| POST | `/api/patrol-routes/:id` | aggiorna `{name, site_id?, window_minutes?, active?}` |
| DELETE | `/api/patrol-routes/:id` | elimina (cascade) |
| POST | `/api/patrol-routes/:id/checkpoints` | aggiungi `{name, kind, code?/lat?/lon?/radius_m?}` |
| POST | `/api/patrol-checkpoints/:id` | aggiorna checkpoint |
| DELETE | `/api/patrol-checkpoints/:id` | elimina checkpoint |

Esecuzione (richiede modulo `giri`; qualunque utente del tenant; il pianificatore vede tutte le
ronde, gli altri solo le proprie):

| Metodo | Rotta | Azione |
|---|---|---|
| POST | `/api/patrol-routes/:id/rounds` | avvia una ronda (ritorna checkpoint + progresso) |
| GET | `/api/patrol-rounds` | elenco ronde |
| GET | `/api/patrol-rounds/:id` | dettaglio + punzonature + compliance (`done`/`total`) |
| POST | `/api/patrol-rounds/:id/punch` | punzona `{checkpoint_id, code?/lat?/lon?, note?}` → `{ok, reason, round}` |
| POST | `/api/patrol-rounds/:id/close` | chiudi → `completato` se tutti i punti OK entro la finestra, altrimenti `incompleto` |

Validazione: tipo fuori catalogo → 400; gps senza coord → 400; qr/nfc/beacon senza codice → 400.

## UI

- **Console → Lavoro sul campo → Ronde**: il pianificatore crea giri, gestisce i checkpoint
  (QR/NFC/beacon con codice, GPS con lat/lon/raggio) e rivede le ronde eseguite con esito per
  checkpoint e compliance.
- **PWA «Oggi» (`/worker/`)**: sezione **Ronde** (visibile solo se il modulo è attivo); l'operatore
  avvia la ronda, punzona ogni checkpoint, vede il progresso e chiude il giro.
  - **QR**: scanner **nativo** con fotocamera (`BarcodeDetector`) — overlay con mirino, timeout 30s;
    bottone «Annulla / digita» per l'inserimento manuale.
  - **NFC**: lettura tag **nativa** (`NDEFReader`, Web NFC su Android/Chrome).
  - **beacon**: inserimento codice manuale.
  - **GPS**: geolocalizzazione del browser.
  - Dove lo scanner non è supportato (browser/permessi), si ripiega automaticamente sull'input
    manuale del codice. La verifica resta lato server (match codice / distanza GPS).

## Prove

`qa/patrols-e2e.sh` **14/14**: config giro+checkpoint (qr+gps), validazione, ACL config, avvio
ronda, punch QR ok/ko, punch GPS dentro/fuori raggio, chiusura completato/incompleto, compliance
2/2, isolamento tenant, gating modulo off → 403. Regressione verde (field-slice1 8, custom-roles
16, loneworker 15).

## Perimetro

Ronde = **Lavoro sul campo**, non Sicurezza: niente escalation/SOS/reperibilità. Riusa `sites`
(facoltativo) e i geofence come riferimento d'area; indipendente dagli altri moduli campo.
Prossimo possibile: scanner QR/NFC nativo in-app, checkpoint riusabili fra giri, finestra per
fascia oraria/turno.
