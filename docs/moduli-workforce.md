# Modulo «Forza lavoro e turni» (workforce) — perimetro e visione

Analisi e disegno. Nessuna implementazione in questo documento. Principio guida:
**un prodotto è potente quanto più gestisce da solo la complessità che l'utente non vede**.
L'utente fa pochi passi semplici; il motore sotto fa il lavoro duro (rotazioni, conflitti,
coperture, proposte). Usabilità = semplice in superficie, intelligente sotto.

## 1. Cosa è il modulo

Gestione del **chi-lavora-quando**: pianificare i turni, pubblicarli, vedere chi è in servizio,
timbrare la presenza, organizzare le persone in squadre, dichiarare disponibilità e assenze.
È la capacità **organizzativa** della forza lavoro — distinta da: *cosa* si fa (Interventi), *quanto*
si rendiconta (Consuntivi), *sorveglianza* (Giri), *sicurezza* (Lone worker / reperibilità d'allarme).

Confine netto: workforce = **turno pianificato + presenza + squadre + disponibilità/assenze**.
Non possiede ordini, non possiede la valorizzazione ore, non possiede la reperibilità d'allarme.

## 2. Perimetro oggi (cosa possiede)

| Funzione | Tabelle | Endpoint | Gate |
|---|---|---|---|
| Pianificazione turni | `shift_plans` | `/api/shift-plans` | `workforce` + cap `plan` |
| Presenza (timbratura) | `shifts` | `/api/shift/start`·`/end`·`/current` | `workforce` |
| Squadre | `teams`,`team_members` | `/api/teams` | `workforce` |
| Disponibilità/reperibilità semplice | `oncall` | `/api/oncall` (CRUD: create/**update**/delete) | `workforce` + `plan` |
| Assenze/indisponibilità | `worker_absences` | `/api/absences` (CRUD: create/**update**/delete) | `workforce` + `plan` |
| Copertura minima | `coverage_requirements` | `/api/coverage` (CRUD: create/**update**/delete) | `workforce` + `plan` |
| Config turno | `module_config workforce.shift` | — | `workforce` |
| Widget dashboard | — | widget `shifts` | `workforce` |

Rimosso il modulo → spariscono (menu + API 403): turni, presenza, squadre, disponibilità, widget
shifts. **Non tocca** Consuntivi (modulo a sé, consuntivo manuale regge senza workforce), Interventi,
e la reperibilità d'allarme (perimetro Sicurezza). Il Diario è condiviso `workforce OR interventi`.

## 3. Servizi COMUNI dell'area «Lavoro sul campo»

Due capacità non sono di workforce soltanto: servono a **tutta** l'area Campo (Forza lavoro e turni,
Ronde e punzonature, Interventi e cantieri). Le progettiamo come **servizi core condivisi**, non
dentro un singolo modulo, così si mettono a fattor comune senza duplicarle e senza accoppiare i
moduli fra loro.

### 3a. Calendario / Agenda (servizio core condiviso)

Un **unico calendario** (griglia giorno / settimana / mese, stile Google) che mostra, a livelli
sovrapponibili e filtrabili, gli eventi di **ogni modulo Campo attivo**:

- **Forza lavoro e turni** → turni pianificati + presenza.
- **Interventi e cantieri** → appuntamenti/visite degli ordini (finestra programmata `scheduled_*`).
- **Ronde e punzonature** → giri pianificati / finestre di ronda.

Ogni modulo **contribuisce una "sorgente eventi"** (un adapter che espone i suoi record come eventi
`{titolo, inizio, fine, persona/squadra, sede, colore, link}`); il calendario core li fonde e li
filtra per persona/squadra/sede/modulo. Il calendario **esiste finché è attivo almeno un modulo che
lo alimenta**; spegnere un modulo fa sparire solo il suo livello, non il calendario.

Resa: **FullCalendar Standard (MIT)** (giorno/settimana/mese bastano; niente timeline/risorse
"premium"), conflitti in rosso, drag per spostare/estendere (chi pianifica). Un solo componente,
riusato dalle tre voci di menu — non tre calendari diversi.

**Agende personali (Google / Microsoft / Apple) — self-contained.** Ogni dipendente ha **un** URL
iCal (ICS) personale a sottoscrizione (`/api/calendar/<token>.ics`) che incolla una volta nel suo
calendario: dentro ci finiscono **tutti** i suoi item dai moduli attivi (turni, sue visite, sue
ronde) — non un feed per modulo, ma uno per persona. Read-only, token revocabile. Stesso meccanismo
iCal già usato per la reperibilità d'allarme: lo generalizziamo come servizio core. Zero credenziali,
zero OAuth, funziona con tutti i calendari, coerente con la direzione "self-contained".
Push OAuth bidirezionale verso Google/Microsoft = opzione futura opt-in, non cambia il modello base.

### 3b. Motore regole / assegnazione (servizio core condiviso)

Vedi §4: lo stesso motore che valida/propone i turni serve anche a reperibilità d'allarme,
assegnazione ordini e pianificazione giri. È core, non di workforce.

## 4. Motore regole dei turni (il cuore "invisibile")

Un **motore di vincoli/assegnazione nativo**, separato e **riusabile da altri moduli**
(reperibilità d'allarme, assegnazione ordini, pianificazione giri): stesso motore, dati diversi.
Prende persone + disponibilità + competenze + copertura richiesta e produce/valida un piano.

**Regole proposte (dall'esperienza di workforce scheduling), tutte con default sensati:**

*Vincoli duri (non violabili):*
- **Riposo minimo tra turni** (es. 11h) — normativa/benessere.
- **Max ore/settimana** e **max giorni consecutivi**.
- **Competenze richieste**: un turno che richiede una qualifica va solo a chi ce l'ha (aggancio al
  modulo **`qualifiche`**).
- **Disponibilità/assenze**: niente turni su ferie/indisponibilità dichiarate.
- **Un turno per persona per slot** (niente sovrapposizioni) — oggi mancante, va aggiunto.
- **Copertura minima**: ogni sede/fascia deve avere N persone (e le competenze giuste).

*Vincoli morbidi (preferenze, ottimizzate):*
- **Equità**: distribuzione bilanciata di ore, notti, weekend, festivi.
- **Continuità**: evitare turni "spezzati"; preferire la stessa persona sullo stesso sito/cliente.
- **Preferenze personali** (orari preferiti, max notti).
- **Costo**: minimizzare straordinari (aggancio futuro al listino di Consuntivi).
- **Anzianità/seniority** dove serve una guida di squadra.

**Come resta semplice:** l'utente non configura venti parametri. Sceglie un **preset** ("24/7 a 3
turni", "solo giorni feriali", "copertura weekend") e il motore applica i default; l'avanzato è
nascosto dietro "regole fini" per chi le vuole. Il prodotto **rileva e segnala** i conflitti da solo
(oggi non lo fa) e **propone** come risolverli.

**Implementazione:** prima un **motore nativo** a regole (deterministico, spiegabile, zero nuove
dipendenze) — riusa il pattern del motore escalation nativo già in casa. Per l'ottimizzazione pesante
(suggerire interi turni rispettando molti vincoli) resta **opzionale** un solver esterno
**Timefold Community (Apache-2.0)** come fase successiva, non obbligatorio (vedi [consuntivi-plan]).
Il motore espone un contratto comune (`propose`, `validate`, `explain`) che anche gli altri moduli
possono chiamare.

## 5. AI come strumento per i turni

L'AI è **attore** anche qui, con il pattern già in casa (grounding sui dati del tenant + tool-calling,
**scritture sensibili con conferma**, mai azioni critiche automatiche):

- **Comporre un piano a parole**: «copri il cantiere Milano lun-ven 8-17 con 2 persone, almeno una
  con patentino» → l'AI propone una bozza di turni (che il pianificatore rivede e pubblica).
- **Cambi turno**: «Mario dà buca martedì» → l'AI propone i sostituti validi (disponibili + competenti
  + senza violare i vincoli) ordinati per equità, con un clic per applicare.
- **Spiegare**: «perché questo turno è in conflitto?» → risposta in chiaro (es. "viola il riposo di
  11h con il turno di ieri").
- **Riempire i buchi**: segnala coperture mancanti e propone chi metterci.

L'AI **propone**, l'uomo **pubblica**. Nessun turno va live senza conferma di chi pianifica. L'AI usa
il motore regole del §4 come strumento (non inventa: valida contro i vincoli reali).

## 6. Distinzione dei registri (da tenere pulita)

Quattro registri **distinti**, da collegare ma non confondere:
`shift_plans` (pianificato) → `shifts` (presenza timbrata) → `timesheets` (ore approvate, modulo
Consuntivi) → `checkins` (sessione lone worker, Sicurezza). Il flusso ideale
**piano → presenza → consuntivo** li collega (derivazione automatica proposta, confermabile), senza
fonderli: ogni registro resta la verità del suo dominio.

## 6bis. Contorni e ciclo di vita del modulo (attivabile/disattivabile pulito)

Ogni modulo Campo deve accendersi e spegnersi **senza rompere nulla** e senza lasciare dati orfani.
Regole di contorno:

- **Dati di proprietà esclusiva.** Ogni modulo possiede le SUE tabelle (RLS per tenant, FK con
  `ON DELETE CASCADE` interne). Workforce: `shift_plans`, `shifts`, `teams`, `team_members`, `oncall`.
  Nessun'altra funzione scrive in queste tabelle; nessun modulo dipende dalle tabelle di un altro via
  FK rigida (i legami cross-modulo sono **deboli**: id nullable + `ON DELETE SET NULL`, come
  `work_orders.assigned_group`). Così spegnere un modulo non rompe gli altri.
- **Tre stati, non due.** (1) **Attivo**. (2) **Disattivato** = reversibile: le funzioni e il menu
  spariscono, le API rispondono 403 (già così, `module-gating`), **ma i dati restano** nel DB e
  ritornano riaccendendo il modulo. (3) **Rimosso/Purga** = irreversibile: cancella i dati del
  modulo per quel tenant.
- **Avviso perdita dati.** La **disattivazione** NON perde dati (si limita a nasconderli) e lo dice:
  «I dati dei turni restano salvati; riattivando il modulo tornano disponibili». La **Purga** invece
  è distruttiva e va dietro una conferma esplicita che **conta i record**: «Eliminerai
  definitivamente N turni, M presenze, K squadre di questo tenant. Operazione irreversibile.»
  (digita per confermare). Niente purga silenziosa.
- **Blocchi di sicurezza alla disattivazione** (come già per `loneworker` con incidenti aperti):
  avvisare se ci sono **turni futuri pubblicati** o **presenze aperte** (turni timbrati non chiusi)
  prima di spegnere, con l'opzione «chiudi/annulla prima».
- **Servizi condivisi (calendario, motore regole) NON si spengono con un singolo modulo.** Il
  calendario resta finché c'è almeno un modulo che lo alimenta; perde solo il livello del modulo
  spento. Il feed iCal personale smette di includere gli item di quel modulo, il resto continua.
- **Reversibilità provabile** (già nella pratica del repo): spegnere il flag + riavviare la build
  precedente sul DB migrato (migrazioni additive) non deve rompere nulla.

## 7. Cose da sistemare nel perimetro (decisioni aperte)

1. **Doppia "reperibilità"**: `oncall` (workforce) vs rotazioni d'allarme (Sicurezza). Rinominare la
   prima in **«Disponibilità»** per togliere l'ambiguità.
2. **Manifest**: workforce dichiara `provides:["shifts","oncall"]` → correggere in
   `["shifts","teams","availability"]` (la reperibilità d'allarme è della Sicurezza).
3. **Squadre orfane**: oggi `teams` non è consumato dalla pianificazione. Collegarle ai turni
   (turno di squadra) o trattarle come primitiva condivisa.
4. **Capability `plan` unica** per turni/ordini/giri: valutare split (`shifts.plan`/`orders.plan`/
   `patrols.plan`) se servono deleghe separate.
5. **Conflitti/overlap e disponibilità**: oggi la creazione turno controlla solo `fine>inizio`.
   Il motore §4 colma questo buco (è la base di tutto il resto).

## 8. Roadmap a slice (proposta, non avviata)

- **W1 — Calendario + iCal come SERVIZIO CORE condiviso. FATTO.** Vista FullCalendar
  (giorno/settimana/mese, voce menu «Calendario» sotto Lavoro sul campo) col contratto "sorgente
  eventi": adapter attivi per **workforce** (`shift_plans`), **interventi** (ordini con
  `scheduled_start`) e **giri** (ronde eseguite); ogni sorgente è inclusa solo se il suo modulo è
  attivo (tenant RLS; i ruoli ristretti vedono solo i propri). Endpoint: `GET /api/calendar/events`,
  `GET /api/calendar/my-url`, `GET /api/calendar.ics?token=` (feed iCal **personale**, un feed per
  persona che aggrega tutti i moduli attivi; senza sessione, token revocabile). Mig `0081`
  (`calendar_ical_tokens`). e2e `qa/calendar-e2e.sh` 4/4. Resta per dopo: pubblicazione vera
  (draft→publish) e il drag per spostare/estendere.
- **W2 — Motore regole (nativo). FATTO (prima tappa).** Engine PURO e RIUSABILE in
  `core/src/scheduling.rs` (`validate(cand, existing, rules) -> [violation]`, deterministico,
  spiegabile, con test unitari): **overlap = vincolo DURO** (blocca la creazione del turno, 409 con
  spiegazione), **riposo minimo** e **max ore/settimana** = avvisi (non bloccano, tornano nella
  risposta). Config per-tenant `workforce.rules` con default di prodotto (preset "standard":
  riposo 11h, 48h/sett). Endpoint preflight `POST /api/shift-plans/validate` (riusabile dalla UI).
  Console: alla pianificazione gli avvisi appaiono in toast e un conflitto duro offre «Forza»
  (override consapevole via header `x-force`). e2e `qa/scheduling-e2e.sh` 5/5.
  Resta per dopo: competenze (via `qualifiche`), copertura minima, consecutivi, e l'uso del motore
  da parte di reperibilità/ordini/giri.
- **W3 — Disponibilità/assenze** + competenze (`qualifiche`) + equità.
  - **W3a — Assenze FATTO.** Tabella `worker_absences` (ferie/malattia/permesso/assenza, RLS, mig
    `0082`); un'assenza è un vincolo **DURO** nel motore (niente turni sopra un'assenza). CRUD
    `GET/POST /api/absences`, `DELETE /api/absences/:id` (pianificatore). Pannello «Assenze e
    indisponibilità» nella pagina Turni. e2e `qa/absence-e2e.sh` 5/5.
  - **W3b — Competenze FATTO.** Modulo `qualifiche` attivato (mig `0083`): catalogo `qualifications`
    + assegnazione `user_qualifications` (con scadenza) + `shift_plans.required_qualification`. Regola
    motore **DURA** `qualification`: un turno che richiede una qualifica va solo a chi la possiede
    valida (non scaduta) all'orario del turno. CRUD `/api/qualifications`, assegnazione
    `/api/users/:id/qualifications`. Console: card «Qualifiche e competenze» in Turni (crea/assegna)
    + «Competenza richiesta» nel form turno. e2e `qa/qualification-e2e.sh` 5/5.
  - **W3c — Copertura minima + equità FATTO.** `coverage_requirements` (sede/giorni/fascia/min
    persone/qualifica opz., mig `0084`): `GET/POST/DELETE /api/coverage` + `GET /api/coverage/check`
    (per requisito e giorno conta chi copre l'INTERA fascia con sede/qualifica giuste e segnala i
    giorni sotto il minimo — i "buchi"; filtro `?requirement=<id>`). Equità (morbida, informativa):
    `GET /api/fairness` = carico per persona (ore/turni/weekend/notti) + squilibrio max-min. Console:
    card «Copertura minima» in Turni (requisiti + «verifica copertura» che elenca i buchi + «equità»).
    e2e `qa/coverage-e2e.sh` 4/4.
- **W4 — AI turni. FATTO (prima tappa: sostituti).** `GET /api/shift-plans/:id/substitutes`:
  proposta **grounded** di sostituti per un cambio turno — il MOTORE scarta chi ha conflitti/assenze
  o manca la competenza, e ordina per equità (meno ore). Se youworker.ai è attivo aggiunge una frase
  di consiglio in linguaggio naturale (fallback: testo deterministico). L'AI **propone**, il
  pianificatore **assegna** (un clic riassegna: nuovo turno al sostituto + annulla il vecchio).
  Console: azione «sostituti» sul turno. e2e `qa/substitutes-e2e.sh` 4/4.
  Resta per dopo: comporre un intero piano a parole, riempimento automatico dei buchi di copertura.
- **W5 — Calendario pianificatore FATTO.** Strumenti per chi fa gli orari, nel calendario:
  - **Drag & drop / resize** dei turni (sposta/ridimensiona come un appuntamento): endpoint
    `POST /api/shift-plans/:id/move` validato dal motore (esclude il turno stesso dai conflitti;
    header `x-force` per forzare).
  - **Colore per lavoratore** (`users.cal_color`, mig `0085`; fallback palette stabile): ogni
    turnista ha un colore distinto; endpoint `POST /api/users/:id/color`. **Legenda** con
    checkbox **mostra/nascondi** per persona (come i calendari Google) + color picker + «tutti/nessuno».
  - **Export** della vista (filtrata ai selezionati): **CSV** (Excel) e **PDF** (jsPDF).
  - **Genera turni**: applica un modello a più lavoratori su un intervallo; salta in automatico
    conflitti/assenze (motore).
  - **✦ Suggerisci (AI)**: `POST /api/shift-plans/suggest` propone combinazioni VALIDE e bilanciate
    (equità, carico aggiornato man mano) da rivedere e applicare; frase youworker.ai se attivo.
  - Verificato: color/move/suggest 200, calendario con colori distinti + legenda + tools.
- Oltre: push OAuth bidirezionale e/o solver Timefold per ottimizzazione pesante — solo se richiesto.

Vedi anche [architettura-moduli.md](architettura-moduli.md), [consuntivi-plan], [field-work-module].
