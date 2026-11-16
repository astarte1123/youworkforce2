# Lavoro sul campo — modalità Cantieri vs Assistenza (analisi)

> **SUPERATO (2026-11-16).** L'idea del toggle di "modalità" su un modello unico si è rivelata una
> scorciatoia: cantiere e assistenza sono due mestieri diversi. La direzione definitiva è **due
> moduli distinti** (`interventi`=Assistenza, `cantieri`=Cantieri) su core condiviso. Vedi il piano
> operativo in `docs/lavoro-campo-piano.md`. Questo file resta come analisi storica del dominio.

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

## DECISIONE (2026-11-16): due moduli su core condiviso

La modalità "solo etichetta" su `work_orders` **non bastava**: cantiere e assistenza divergono
in entità e flusso, non solo nei nomi. `work_orders` (una finestra programmata + report + firma)
descrive bene **una visita di assistenza**, non un cantiere (manca il progetto-padre,
l'avanzamento, le lavorazioni, le presenze). Per questo "Nuovo cantiere" e "Nuovo appuntamento"
risultavano identici.

Scelta: **due moduli distinti** che condividono il core campo (persone, sedi, calendario,
motore turni, allegati/firma, consuntivi, AI, PWA, qualifiche):

| | Modulo `interventi` (= Assistenza) | Modulo `cantieri` (nuovo) |
|---|---|---|
| Oggetto | ordine/appuntamento (`work_orders`) | **commessa** (progetto) |
| Scomposizione | checklist della visita | **lavorazioni (WBS)** con peso+avanzamento |
| Avanzamento | esito per visita | **media pesata** delle lavorazioni + **SAL** datati |
| Commerciale | listino + consuntivo per visita | SAL/milestone, importo contratto |
| Pianificazione | agenda appuntamenti | turni (riusa workforce) legati alla commessa |

Un tenant attiva `interventi`, `cantieri` o entrambi (dal superadmin). Aree di menu separate
("Assistenza" e "Cantieri") perché flussi e spesso squadre sono diversi.

### Implementato (modulo Cantieri)
- mig `0088`: catalogo `cantieri`; tabelle `commesse`, `commessa_lavorazioni` (peso, progress_pct,
  status), `commessa_sal` (data, pct, importo); `shift_plans.commessa_id` (FK ON DELETE SET NULL).
  mig `0089`: backfill righe `tenant_modules` per i tenant esistenti.
- Fix robusto: `set_module` ora fa **UPSERT** (un modulo aggiunto dopo la creazione del tenant
  non aveva la riga → l'attivazione falliva silenziosamente).
- Avanzamento commessa = `round(sum(weight*progress)/sum(weight))`. Status lavorazione `fatto`→100%.
- Endpoint: `GET/POST /api/commesse`, `GET/POST/DELETE /api/commesse/:id`,
  `POST /api/commesse/:id/lavorazioni`, `POST/DELETE /api/lavorazioni/:id`,
  `POST /api/commesse/:id/sal`. Turno può avere `commessa_id`; lista turni e calendario mostrano la commessa.
- Console: menu "Cantieri" (vCantieri) con elenco+barre avanzamento; drawer commessa con lavorazioni
  (slider avanzamento), SAL, turni legati; form turno con select "Cantiere / commessa". e2e `qa/cantieri-e2e.sh` 9/9.

Nota: il toggle `interventi.modes` (cantieri/assistenza) resta per la terminologia del modulo
interventi, ma la vera separazione ora è a livello di **modulo**. Candidato a deprecazione.

## Fuori da questa slice (roadmap)
- Anagrafica **cliente/commessa** dedicata (oggi solo sede).
- Vista «agenda appuntamenti» per l'assistenza (diversa dalla copertura-cantiere).
- Requisiti di copertura per **intervento** (oltre che per sede).
- Suggeritore AI che assegna tecnici agli appuntamenti per prossimità/competenza/carico.
