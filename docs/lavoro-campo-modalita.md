# Lavoro sul campo — modalità Cantieri vs Assistenza (analisi)

Il lavoro sul campo non è uno solo. Due realtà operative diverse convivono sotto l'area «Lavoro sul
campo», e un tenant può averne una, l'altra o entrambe. L'amministratore del tenant **attiva la/e
modalità** (impostazione per-tenant, non del superadmin): cambia terminologia, campi e strumenti
mostrati, senza moltiplicare i moduli.

## Le due modalità

### 🏗️ Cantieri (lavoro continuativo su un sito)
- Il lavoro è legato a un **luogo durevole** (il cantiere = una **sede**): più persone, per giorni o
  settimane, a **copertura** di fasce orarie.
- La domanda chiave è *"il cantiere è coperto?"* → **turni per cantiere** + **requisiti di copertura
  minima** per sede/fascia (già in workforce W3c).
- Esecuzione: presenza/timbratura, squadre, competenze (es. patentino). Poco "appuntamento", molto
  "turno + copertura".
- Entità: **sede** (cantiere) + **turni** su quella sede. L'eventuale `work_order` è il cantiere stesso
  o una lavorazione di cantiere.

### 🔧 Assistenza / Manutenzione (tecnico con appuntamenti)
- Il tecnico va **dal cliente** (azienda/casa): tanti **appuntamenti** brevi, ciascuno con orario,
  indirizzo, rapporto, materiali, **listino** e **consuntivo** (ore/spese) → fatturazione.
- La domanda chiave è *"chi va dove e quando, e quanto si rendiconta?"* → **ordini/appuntamenti**
  (work_orders con `scheduled_start/end`, report, firma) + **Consuntivi** (listino = slice 3).
- Esecuzione: arrivo/avvio, esito (`outcome_kind`), allegati/firma, materiali, valorizzazione.
- Entità: **cliente/commessa** (oggi solo sede) + **ordine/appuntamento** + **consuntivo**.

## Mappa su ciò che esiste

| Concetto | Cantieri | Assistenza |
|---|---|---|
| Dove | sede (cantiere) | cliente/sede dell'appuntamento |
| Unità di lavoro | turno a copertura | ordine/appuntamento (visita) |
| Pianificazione | turni + copertura per sede | agenda appuntamenti + assegnazione |
| Rendicontazione | ore/presenza | consuntivo (ore+spese) + listino |
| Moduli coinvolti | workforce (+ interventi per le lavorazioni) | interventi + consuntivi (+ qualifiche) |

Il modello dati **work_orders** regge entrambi (ha sede, finestra programmata, report, esito,
allegati, link a consuntivo). La differenza è **di workflow e di presentazione**, non di tabelle: per
questo la scelta è un'**impostazione di modalità** del tenant, non due moduli separati.

## Design della scelta (per-tenant)

Config `interventi.modes = { "cantieri": bool, "assistenza": bool }` (default: entrambe), impostabile
dal **tenant_admin**. Effetti:
- **Terminologia**: l'unità di lavoro si chiama «Cantiere» (solo cantieri), «Intervento/Appuntamento»
  (solo assistenza) o «Intervento / Cantiere» (entrambe).
- **Campi/strumenti enfatizzati**: assistenza → appuntamento (data/ora), listino, consuntivo; cantiere
  → turni + copertura per sede.
- **Legame turno ↔ intervento/cantiere**: un turno può essere associato a un `work_order`
  (`shift_plans.work_order_id`): «questo turno è per il cantiere/intervento X». Visibile su calendario
  e sull'intervento.

## Legame turno ↔ intervento (implementato in questa slice)
- `shift_plans.work_order_id` (FK, `ON DELETE SET NULL`).
- Form «Pianifica turno»: campo **Intervento/Cantiere** (ordini aperti), etichetta secondo le modalità.
- Calendario: il turno mostra anche l'intervento collegato.

## Fuori da questa slice (roadmap)
- Anagrafica **cliente/commessa** dedicata (oggi solo sede).
- Vista «agenda appuntamenti» per l'assistenza (diversa dalla copertura-cantiere).
- Requisiti di copertura per **intervento** (oltre che per sede).
- Suggeritore AI che assegna tecnici agli appuntamenti per prossimità/competenza/carico.
