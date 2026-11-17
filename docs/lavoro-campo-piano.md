# Lavoro sul campo — piano di implementazione (Assistenza + Cantieri)

Documento di pianificazione. Obiettivo: due moduli **distinti e di pari dignità** nella sezione
«Lavoro sul campo», ciascuno fedele al proprio mestiere, costruiti sul **core condiviso** di
YouWorkforce senza duplicazioni e senza scorciatoie.

## 0. Principi (vincolanti)

1. **Nativo e self-contained**: nessuna dipendenza esterna salvo l'AI. Motori nativi.
2. **Core condiviso, non duplicato**: persone/ruoli/gruppi, sedi+geofence, calendario, motore
   turni/scheduling, allegati+firma, consuntivi, qualifiche, PWA, dashboard, AI, audit/RLS.
3. **Separazione per MODULO**, non per flag di etichetta. Un tenant attiva Assistenza, Cantieri o
   entrambi dal superadmin; ogni modulo ha menu, entità, flusso e widget propri.
4. **AI attore in ogni processo** (proponi → l'uomo pubblica), mai l'unico decisore.
5. **Isolamento tenant assoluto** (RLS + tenant_id esplicito su owner_pool).
6. **Doc + e2e a ogni slice**, nello stesso commit del codice.
7. **UI comprensibile**: nomi chiari, spiegazioni, niente codici grezzi.

## 1. Pulizia delle scorciatoie (Fase 0 — obbligatoria, prima di tutto)

Da rimuovere perché umiliano il prodotto:

- `interventi.modes` (tool `modes`, endpoint `/api/field-modes`, `fieldTerms()`, `modesCard`,
  banner di modalità, campo "Intervento/Cantiere" ambiguo nel form turno). La separazione è il
  modulo, non l'etichetta.
- Identità mista del modulo `interventi`: diventa **solo Assistenza**.
- Mantengo: `shift_plans.work_order_id` (turno↔appuntamento, legittimo) e
  `shift_plans.commessa_id` (turno↔commessa, legittimo).

Esito Fase 0: due moduli puliti già visibili in menu — **Assistenza** e **Cantieri** — senza
residui di modalità. Migrazione additiva che ritira il tool `modes` (no drop distruttivi:
`DELETE FROM module_tools WHERE module_code='interventi' AND tool_code='modes'` + pulizia config).

---

## 2. Modulo ASSISTENZA (field service / manutenzione a visite)

Mestiere: il tecnico va dal cliente; tante visite brevi, ciascuna con orario, rapporto, materiali,
listino e consuntivo → fatturazione per intervento/contratto.

### Stato attuale

work_orders (titolo, sede, assegnatario, finestra programmata, report, firma, checklist,
materiali, esito, allegati). PWA «Oggi» con arrivo/avvio/esito/firma. Consuntivi (modulo separato).

### Lacune vs un vero field-service

- **Cliente** come anagrafica (oggi solo "sede"): ragione sociale, sedi multiple, referenti,
  recapiti, condizioni.
- **Contratto / SLA**: tempo di risposta, manutenzioni ricorrenti, monte-ore.
- **Agenda di dispatch**: vista per tecnico/giorno, assegnazione rapida, zone.
- **Listino** e valorizzazione: voci (manodopera/materiali/trasferta), prezzo, sconti → consuntivo
  per visita → documento.
- **Ricorrenze**: contratti di manutenzione che generano appuntamenti periodici.
- **Richieste/ticket**: canale d'ingresso prima dell'ordine.

### Piano Assistenza (slice)

| Slice | Contenuto | Entità/mig | Endpoint | UI | AI |
|---|---|---|---|---|---|
| A1 | **Cliente** anagrafica | `clients`, `client_contacts`; work_orders.client_id | CRUD clienti | menu "Clienti" o sezione; select cliente nell'ordine | — |
| A2 | **Listino + valorizzazione visita** | `price_list_items`, `work_order_lines` | CRUD listino; righe su ordine | listino in Config; righe nel rapporto; totale | bozza righe da testo rapporto |
| A3 | **Agenda dispatch** | (nessuna nuova) | `/api/orders/agenda?tech&day` | vista calendario per tecnico + drag assegna | suggerisci tecnico per prossimità/competenza/carico |
| A4 | **Contratto/SLA + ricorrenze** | `service_contracts`, `maintenance_plans` | CRUD; generatore appuntamenti | contratti; "SLA a rischio" | genera piano manutenzione |
| A5 | **Richieste/ticket** | `service_requests` | CRUD; richiesta→ordine | inbox richieste | triage/priorità |

Dashboard widgets: appuntamenti oggi, SLA a rischio, consuntivi da approvare, carico per tecnico.

---

## 3. Modulo CANTIERI (construction / lavoro continuativo su sito)

Mestiere: progetto lungo su un sito; squadre su turni a copertura; avanzamento per lavorazioni;
SAL per fatturazione a stato avanzamento; costi (ore+materiali) vs budget.

### Stato attuale (già realizzato)

Commessa → lavorazioni (WBS, media pesata) → SAL; turni legati alla commessa. CRUD + e2e 9/9.

### Lacune vs un vero gestionale di cantiere

- **Presenze/timbrature** in cantiere (entrata/uscita, geofence del sito).
- **Squadre** assegnate alla commessa (riuso teams core).
- **Cronoprogramma** (date per lavorazione, dipendenze, vista Gantt).
- **Costi**: consuntivo ore (da turni/presenze) + materiali/DDT vs **budget** → scostamento.
- **Giornale di cantiere** (diario datato: meteo, presenti, attività, note; riuso diario).
- **Sicurezza**: riferimenti POS/PiMUS, DPI, riunioni (aggancio a modulo prevenzione).
- **Fatturazione da SAL**: dal SAL al documento/consuntivo.

### Piano Cantieri (slice)

| Slice | Contenuto | Entità/mig | Endpoint | UI | AI |
|---|---|---|---|---|---|
| C1 | **Squadre + presenze** | commessa_teams (riuso teams); `site_attendance` (clock-in geofence) | assegna squadra; clock-in/out | squadra sulla commessa; timbratura PWA | — |
| C2 | **Cronoprogramma** | lavorazioni += start/end/deps | `/commesse/:id/gantt` | vista Gantt WBS | rileva ritardi, ripianifica |
| C3 | **Costi vs budget** | `commessa_costi` (materiali); ore da turni/presenze | report costi | scostamento budget | spiega scostamenti |
| C4 | **Giornale di cantiere** | riuso `diary_entries` con commessa_id | diario per commessa | giornale datato | bozza giornale vocale/testo |
| C5 | **Fatturazione da SAL** | sal→consuntivo/documento | genera consuntivo da SAL | pulsante "fattura SAL" | — |

Dashboard widgets: avanzamento commesse, scostamento budget, presenze oggi, SAL del mese.

---

## 4. Contratto del core condiviso (cosa riusano entrambi)

- **Persone/ruoli/gruppi/capability** (core.identity/groups/iam) — `plan`, `approve`, ecc.
- **Sedi + geofence** (core.sites) — luogo di cantiere e indirizzo cliente.
- **Calendario** (core) — appuntamenti (assistenza), turni di commessa (cantieri).
- **Motore turni/scheduling** (`scheduling.rs`) — turni, copertura, sostituti, equità.
- **Allegati + firma** (pattern bytea) — rapporti, foto, SAL firmati.
- **Consuntivi** (`timesheets`) — valorizzazione visita (assistenza) e ore commessa (cantieri).
- **Qualifiche** — competenze richieste su visite e lavorazioni.
- **PWA «Oggi»** — esecuzione sul campo per entrambi.
- **AI** — assegnazioni, bozze, spiegazioni.
- **Dashboard** — widget per-modulo aggregati.
- **Audit/RLS** — isolamento e tracciamento.

Regola: se una funzione serve a entrambi, vive nel **core**; se è specifica del mestiere, vive nel
**modulo**. Nessuna tabella "tuttofare" con campi nullable per far finta.

---

## 5. Ordine di esecuzione proposto

1. **Fase 0** — pulizia scorciatoie (field-modes/modes) + due moduli netti. *(piccola, subito)*
2. **Assistenza A1–A2** (cliente + listino/valorizzazione) — dà corpo reale all'assistenza.
3. **Cantieri C1–C2** (squadre/presenze + cronoprogramma) — dà corpo reale al cantiere.
4. **A3 / C3** (dispatch / costi-budget).
5. **A4–A5 / C4–C5** (contratti-ricorrenze-ticket / giornale-fatturazione SAL).

Ogni slice: migrazione additiva + endpoint gated per modulo + UI comprensibile + e2e + doc + memory,
nello stesso commit. Nessun passo distrugge i dati o umilia il prodotto.

## Stato avanzamento

- ✅ **Fase 0** — scorciatoia modalità rimossa; due moduli netti (commit a5e96e7).
- ✅ **A1 Cliente** — `clients` + `client_contacts` + `work_orders.client_id` (mig 0091); CRUD +
  referenti; menu "Clienti"; select cliente nell'intervento; scheda cliente con i suoi interventi.
  e2e `qa/clients-e2e.sh` 8/8.
- ✅ **C1 Squadre + presenze** — `commessa_teams` (riuso teams core) + `commessa_presenze`
  (timbrature entrata/uscita, geofiltro GPS opz.) (mig 0092); assegna/rimuovi squadra; timbra
  entrata/uscita; scheda commessa con squadre e presenze. e2e `qa/cantieri-squadre-e2e.sh` 9/9.
- ✅ **C2 Cronoprogramma** — lavorazioni con date (start/end) + dipendenza finish-to-start
  (`depends_on`) (mig 0094); vista **Gantt** leggera con barre + avanzamento; rilevamento
  **ritardo** (fine passata e non completata). e2e `qa/cantieri-cronoprogramma-e2e.sh` 7/7.
- ✅ **A2 Listino + valorizzazione** — `price_list_items` (catalogo voci: categoria, unità,
  prezzo) + `work_order_lines` (righe valorizzate sull'intervento, da listino o a mano) (mig 0095);
  menu "Listino"; sezione "Valorizzazione" nell'intervento con totale. e2e `qa/listino-e2e.sh` 7/7.
- ✅ **C3 Costi vs budget** — `commessa_costi` (materiali/noli/subappalti) + `commesse.labor_cost_hour`
  (mig 0096); ore da presenze chiuse × costo orario + materiali vs budget → **scostamento**.
  Scheda commessa: costo orario, voci di costo, riepilogo budget/consuntivo/scostamento.
  e2e `qa/cantieri-costi-e2e.sh` 7/7.
- ✅ **A3 Agenda dispatch** — vista "Agenda tecnici" (da assegnare + carico per tecnico/giorno) +
  **suggerimento tecnico** `GET /orders/:id/suggest-tech` (grounded: esclude assenti nel giorno,
  ordina per carico; youworker.ai opzionale per il riassunto). Assegnazione diretta dal suggerimento.
  e2e `qa/dispatch-e2e.sh` 6/6.
- ⏳ prossimi: A4-A5 (contratti/SLA, ricorrenze, ticket) / C4-C5 (giornale, fatturazione SAL).

## 6. Criteri di "fatto" (definition of done) per slice

- Endpoint con gating modulo + capability + RLS; isolamento tenant verificato.
- UI con nomi chiari e spiegazioni; nessun codice grezzo.
- e2e verde (incluso il gating modulo-off → 403).
- Doc aggiornato; memory aggiornata; nessuna regressione nelle suite vicine.
