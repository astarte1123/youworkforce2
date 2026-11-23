# Configurazione guidata (Setup)

Aiuta l'amministratore del tenant a rendere la piattaforma operativa senza perdersi: una
**checklist "cosa manca"** calcolata dallo stato reale + un **wizard di primo avvio**.

## Come funziona
- **Fonte unica**: `GET /api/setup-status` (solo `tenant_admin`) calcola i passi dallo stato del
  DB e li **condiziona ai moduli attivi** del tenant (non chiede i dispositivi se non c'è un modulo
  dispositivi, non chiede i clienti se non c'è lavoro sul campo, ecc.). Nessun passo hardcoded lato
  UI: la checklist riflette sempre la realtà e si aggiorna da sola man mano che si configura.
- **Checklist persistente**: voce di menu **Configurazione** (gruppo Configurazione, solo admin).
  Mostra avanzamento (`done/total`), e per ogni passo: titolo, *perché serve*, stato attuale e un
  pulsante che porta dritto alla sezione giusta.
- **Wizard primo avvio**: al primo accesso dell'admin, se la configurazione non è completa, si apre
  una guida passo-passo (una sola volta; poi riapribile dal pulsante «Configurazione guidata»).

## Passi calcolati
| key | mostrato se | completo quando |
|-----|-------------|-----------------|
| `sites` | sempre | ≥1 sede |
| `users` | sempre | >1 utente |
| `groups` | loneworker/iot/gps attivo | ≥1 gruppo **e** gruppo on-call predefinito impostato |
| `contacts` | loneworker/iot/gps attivo | ≥1 recapito telefonico verificato |
| `routing` | loneworker attivo | ≥1 regola di instradamento allarmi |
| `rules` | iot attivo | ≥1 regola sensore |
| `devices` | loneworker/iot/gps attivo | ≥1 dispositivo |
| `clients` | cantieri/interventi attivo | ≥1 cliente |

## "Cosa fare adesso" (guida per-ruolo)
Oltre al setup, la **dashboard** mostra in cima una striscia **"Cosa fare adesso"**: azioni operative
in sospeso tagliate sul **ruolo** dell'utente, calcolate dai dati reali — così ognuno atterra sul
proprio lavoro senza cercarlo.
- Fonte: `GET /api/home-tasks` (ogni ruolo tenant). Ritorna `items[]` con `title/detail/link/tone`.
- Mappatura: operatore/responsabile → *allarmi da prendere in carico* (Centrale); reperibile → i
  propri allarmi; pianificatore → *interventi attivi* (Ordini) e *turni da confermare* (Turni);
  lavoratore → *i propri interventi assegnati*. Se non c'è nulla di urgente: "Tutto sotto controllo".
- e2e: `qa/home-tasks-e2e.sh` (struttura, azione allarmi per operatore, contenuto per-ruolo).

## Note
- Endpoint gated: `setup-status` per non-admin riceve `403`; `home-tasks` è per ogni ruolo tenant.
- e2e: `qa/setup-status-e2e.sh` (gating, struttura passi, coerenza `ready`, condizione moduli).
- I passi sono dati, non testo fisso: per aggiungerne uno si estende `setup_status` in `core/src/api.rs`.
