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
| Disponibilità/reperibilità semplice | `oncall` | `/api/oncall` | `workforce` + `plan` |
| Config turno | `module_config workforce.shift` | — | `workforce` |
| Widget dashboard | — | widget `shifts` | `workforce` |

Rimosso il modulo → spariscono (menu + API 403): turni, presenza, squadre, disponibilità, widget
shifts. **Non tocca** Consuntivi (modulo a sé, consuntivo manuale regge senza workforce), Interventi,
e la reperibilità d'allarme (perimetro Sicurezza). Il Diario è condiviso `workforce OR interventi`.

## 3. Calendario stile Google Calendar + sync agende

**Vista (console + PWA).** Un calendario a griglia (giorno / settimana / mese) con i turni come
blocchi colorati per persona o per squadra; filtri per tecnico/squadra/sede; i conflitti evidenziati
in rosso; drag per spostare/estendere un turno (chi pianifica). Niente timeline/risorse "premium".
Riuso diretto di **FullCalendar Standard (MIT)** — viste giorno/settimana/mese bastano (vedi
valutazione OSS in [consuntivi-plan]); nessuna dipendenza a pagamento.

**Collegamento alle agende personali (Google/Microsoft/Apple) — self-contained.**
Strada principale, zero credenziali, zero OAuth: ogni dipendente ha un **URL iCal (ICS) personale,
a sottoscrizione** (`/api/calendar/<token>.ics`) che incolla una volta nel suo Google/Outlook/Apple
Calendar. Da lì i turni (e opzionalmente la reperibilità) compaiono nella sua agenda e si aggiornano
da soli. Read-only, token revocabile, un calendario per persona. È lo **stesso meccanismo iCal già
usato per la reperibilità d'allarme** (`docs/goalert-manuale.md`): lo generalizziamo.

- Pro: funziona con TUTTI i calendari, nessun segreto da custodire, coerente con la direzione
  "self-contained" ([self-contained-native]).
- Limite: è una direzione sola (prodotto → agenda del dipendente). Sufficiente per il 95% dei casi.

**Opzione futura (fase 2, solo se richiesta): push bidirezionale via OAuth** verso Google/Microsoft
(crea/aggiorna eventi nel calendario aziendale, legge disponibilità "busy"). Più potente ma introduce
credenziali e manutenzione: resta **opt-in**, non obbligatoria, e non cambia il modello base iCal.

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

- **W1 — Calendario + iCal.** Vista FullCalendar (giorno/settimana/mese) su `shift_plans` esistenti +
  URL iCal personale per sync Google/Microsoft/Apple. Pubblicazione vera (draft→publish).
- **W2 — Motore regole (nativo).** Vincoli duri (riposo, overlap, competenze, copertura) + rilevazione
  conflitti + preset. Contratto riusabile (`propose`/`validate`/`explain`).
- **W3 — Disponibilità/assenze** + equità (vincoli morbidi) + aggancio competenze (`qualifiche`).
- **W4 — AI turni.** Proposta piano, sostituti per cambi turno, spiegazioni — sul motore W2, con
  conferma umana.
- **W5 (opz.) — Push OAuth** bidirezionale e/o solver Timefold per ottimizzazione pesante. Solo se
  richiesto; non cambia il modello base.

Vedi anche [architettura-moduli.md](architettura-moduli.md), [consuntivi-plan], [field-work-module].
