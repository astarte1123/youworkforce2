# YouWorkforce — Razionalizzazione UI (proposta)

> Scritto per: team prodotto/front-end. Analisi su codice reale (`app.html`, `superadmin.html`,
> `worker/index.html`, `app.css`). Il problema è **architettura dell'informazione + disciplina dei
> pattern**, non lo stile visivo (il design system è buono: chip di stato, drawer, KPI card, toast).

## 0. Il problema in un paragrafo
La console è una **sidebar piatta da 16 voci** senza raggruppamenti, in cui operatività, analisi e
impostazioni admin sono mescolate, le **icone non disambiguano** (Dashboard/Mappa/Cruscotti usano tutte
`home`; Report/Consuntivi/Anagrafiche usano `receipt`; Connettori/Agente/AI/Moduli usano `sliders`), e lo
**stesso concetto è sparso su 3-4 superfici** (gli allarmi vivono in Centrale + Mappa + Report; l'AI in un
fab, una pagina "Agente", una pagina "AI" e ~7 bottoni inline `✦`). Coesistono **due pattern** per lo stesso
compito: i modali non-bloccanti `askConfirm/askPrompt` *e* i `prompt()/confirm()` nativi bloccanti
(bloccati in alcune webview). La PWA del lavoratore è un unico scroll lungo con un **bottone morto**
("Chiedi ai documenti" → toast "non implementato") e **due bottoni di dettatura** in competizione.

## 1. Modello di navigazione proposto (gruppi titolati, admin collassabile)
L'array `NAV` ha già `roles`/`mod`/`feat`: aggiungere un campo `group` e rendere intestazioni di sezione.

| Gruppo | Voci (nuova etichetta ← attuale) | Chi vede |
|---|---|---|
| **Operatività** | Panoramica ← Dashboard · Centrale allarmi · **Mappa** (assorbe Piantine) · Monitoraggio IoT ← Cruscotti IoT | operatore, admin, auditor |
| **Lavoro sul campo** | Interventi ← Ordini di lavoro · Turni · Ore e spese ← Consuntivi | pianificatore, responsabile, lavoratore, reperibile |
| **Analisi** | Report · Assistente AI ← (Agente AI + fab uniti) | per ruolo |
| **Impostazioni** (collassato, solo admin) | Anagrafiche ← Utenti e sedi · Dispositivi · Gruppi ed escalation ← Gruppi allarme · Integrazioni ← Connettori · AI · Moduli e licenze ← Moduli | tenant_admin |

Tre fusioni strutturali: **Piantine → tab dentro Mappa** (segmented `Allarmi · Dispositivi · Indoor`);
**Agente AI + fab → un'unica "Assistente AI"** con switch *Leggi/Agisci*; **ogni voce con icona distinta**.

## 2. tenant_admin
- **Obiettivo:** far *funzionare* il tenant (moduli, connettori, utenti/sedi/dispositivi, gruppi/routing); supervisione quotidiana; riconfig occasionale.
- **Problemi:** vede tutte le 16 voci senza differenziazione; nessun percorso di onboarding; config "split-brain" (metà connessioni stanno nel superadmin → vicoli ciechi "non configurata: il superadmin deve…"); creazioni via `prompt()`.
- **Proposta:** nav a gruppi con **Impostazioni** collassato; **checklist di setup** su Panoramica (① Moduli ✓ ② Connettori ⚠ ③ Utenti/sedi ④ Dispositivi ⑤ Gruppi/escalation) dai dati già fetchati; rinomine (Moduli→"Moduli e licenze", Connettori→"Integrazioni", Gruppi allarme→"Gruppi ed escalation", Utenti e sedi→"Anagrafiche"); negli stati "non configurata (superadmin)" un CTA concreto "[Richiedi configurazione]"; un solo primario "+ Aggiungi dispositivo" (wizard); sostituire i `prompt()` col pattern drawer-form.

## 3. operatore (centrale allarmi) — massima criticità
- **Obiettivo:** vedere l'allarme all'istante e fare triage; ack/assegna/escala/chiudi con audit; localizzare persona/dispositivo; check-in scaduti; report.
- **Problemi:** allarmi spezzati su 3 voci (Centrale lista, Mappa dove, Piantine indoor, Report storico); **"Chiudi con esito" usa `prompt()` bloccante** (rischio sicurezza); drawer incidente sovraccarico (chip, hash UUID grezzo, azione primaria in competizione con 6 secondari); jargon "Ack/Escalate" accanto a "Prendi in carico"; priorità come `●●●`.
- **Proposta:** **Mappa+Piantine unite** + **mini-mappa dentro il drawer incidente** (dati già in `/alarms/map`); **gerarchia azioni**: un primario a tutta larghezza, il resto sotto "Altre azioni"; **sostituire il `prompt()` di chiusura** con modale a esiti preset; **unificare il vocabolario** escalation; nascondere l'UUID dietro "ID tecnico", priorità come chip etichettato; **check-in scaduti come KPI rosso in cima**; dare all'operatore accesso a Dispositivi/Presenza.

## 4. pianificatore
- **Obiettivo:** creare/schedulare ordini e assegnarli; pianificare turni/reperibilità; vedere non assegnati/in ritardo.
- **Problemi:** tre superfici di pianificazione senza calendario condiviso (tutto tabelle di datetime ISO); tre form di creazione quasi identici ma diversi; `ordComplete/ordCancel` via `prompt()`.
- **Proposta:** rinomina **Ordini di lavoro→"Interventi"**; **week-view leggera** (griglia CSS su `/orders`+`/shift-plans`, click apre il drawer con data precompilata); **un solo componente form** per ordine/turno/reperibilità; sostituire i `prompt()`; KPI "Da assegnare" su Panoramica.

## 5. responsabile (approvatore)
- **Obiettivo:** rivedere consuntivi inviati, approvare o rimandare con motivo; vedere la coda; report.
- **Problemi:** il lavoro è una **coda** ma Consuntivi mostra tutto in una tabella piatta senza filtro "in_revisione"; "Chiedi correzioni" via `prompt()`; stati grezzi come chip.
- **Proposta:** default **filtro "Da approvare"** (segmented Da approvare·Tutti·Approvati) + conteggio coda + KPI; sostituire il `prompt()` di correzione con drawer (motivo + flag per riga); rinomina **Consuntivi→"Ore e spese"**; un report ore/costi per l'approvatore.

## 6. lavoratore (PWA) — chi "fa fatica"
- **Obiettivo:** SOS istantaneo; check-in; inizio/fine turno; lavori di oggi; nota (voce/testo), anche offline.
- **Problemi:** **bottone morto** "Chiedi ai documenti" (toast "non implementato"); **due bottoni dettatura** affiancati; `oComplete` via `prompt()` (ambiente più vincolato); **unico scroll lungo** con 4 sezioni temporali; SOS scrolla via; check-in chiede di digitare i minuti + chip jargon.
- **Proposta (ergonomia mobile):** **tab bar in basso (Oggi · Check-in · SOS · Diario)** con **SOS sempre ancorato** (barra rossa/FAB) raggiungibile da ogni tab; **eliminare "Chiedi ai documenti"** finché non c'è RAG; **un solo "🎤 Detta"** (sceglie server/browser STT da solo); **sostituire `oComplete` prompt()** con bottom-sheet a esiti preset; **check-in a chip preset** (15/30/60/custom) + stati in lingua piana; **banner offline globale**; redirect del `lavoratore` che entra in console → `/worker/`.

## 7. reperibile — il buco funzionale — ✅ RISOLTO (commit vista "I miei allarmi")
- **Obiettivo:** sapere quando è reperibile; ricevere/agire sugli allarmi escalati; note.
- **Problema (era strutturale):** gli incidenti possono essere *assegnati a* `reperibile` e GoAlert vi escala, ma non aveva una vista incidenti → la persona a cui l'allarme escalava non poteva vederlo/agirlo in app (solo la notifica GoAlert).
- **Soluzione implementata:** nuova voce menu **"I miei allarmi"** (`vMieiAllarmi`, ruoli `reperibile`+`responsabile`, modulo loneworker): lista incidenti aperti prioritizzati, badge on-call da `/oncall-now`, tocco → drawer incidente con **Prendi in carico** e **Chiudi con esito**. Backend: `ack_incident`/`close_incident` ora ammettono il ruolo `reperibile` (oltre a operatore/tenant_admin); `list_incidents` era già aperto (filtrato per sito). Drawer: `canResp=['operatore','tenant_admin','reperibile']` per ack/close; assegna/nota restano operatore/admin.
- **Residuo:** integrazione con Diario/note della PWA lavoratore (opzionale).

## 8. auditor
- **Obiettivo:** revisione sola lettura di allarmi, movimenti, lavoro, consuntivi, report (compliance).
- **Problemi:** nessun segnale "sola lettura" (i bottoni spariscono in modo incoerente → sembra "rotto"); il suo vero lavoro (report DVR + timeline audit) è una delle 9 voci pari.
- **Proposta:** **badge "Sola lettura"** in topbar; promuovere **Report** in cima + Panoramica = sintesi compliance (incidenti aperti, tempi di risposta già calcolati); stati empty/forbidden che dicono "visibile in sola lettura".

## 9. superadmin
- **Obiettivo:** creare tenant; moduli/licenze; cablare connessioni motori (Traccar/TB/GoAlert/AI); impersonare per supporto; scenari demo.
- **Problemi:** **~10 bottoni per tenant** sulla card principale (muro illeggibile a scala); nessun search/filtro/sort; **due vocabolari** config ("Connessione" vs "Config" vs "Trasporto notifiche" vs "Ambienti"); **tre pattern di conferma** diversi (askConfirm / arm-timer a due click / toggle senza conferma).
- **Proposta:** **collassare ogni tenant in una riga compatta** (logo, nome, chip licenze, semaforo integrazioni — i dot `loadInteg` sono già perfetti), che si espande in dettaglio a **tab** (*Moduli & licenze · Integrazioni · Ambienti demo · Branding*); azioni primarie di riga = Apri · Impersona; **un solo pattern di conferma** (askConfirm per scnRemove e toggle moduli); **search tenant**; rinomine ("Connessione Traccar"→"Traccar · connessione", "Config Traccar"→"Traccar · impostazioni avanzate", "Trasporto notifiche"→"GoAlert · canali (SMS/email)", "Ambienti"→"Scenari demo").

## 10. Cross-cutting
- **10.2 Icone uniche** per ogni voce nav (oggi `home`×3, `receipt`×3, `sliders`×4).
- **10.3 Un pattern per tipo di azione:** conferma → sempre `askConfirm`; input singolo/rinomina/esito → sempre `askPrompt`; create/edit multi-campo → sempre **drawer** con `.cfg-msg`; feedback → `toast`. **Eliminare ogni `prompt()`/`confirm()` nativo** (incClose, ordComplete/Cancel, oComplete worker, grpNew, siteNew/Ren, fpRename, teamNew, addCircle, e soprattutto `assetNew` che ne concatena **tre**).
- **10.4 Geofence/connection:** il popup geofence duplica il pannello laterale → ridurre il popup a "nome + Gestisci →" che focalizza la voce nel pannello (fonte di verità unica). Rendere identici i component health Traccar/ThingsBoard.
- **10.6 Glossario terminologico** (usare sempre la colonna sinistra): Allarme/Incidente; Presa in carico (non "Ack"); Escalation; Reperibilità (non "on-call"); Gruppi ed escalation; Integrazioni (non "Connettori"); Area (non "geofence/zona"); Planimetria (non "piantina"); Interventi (non "Ordini"); Ore e spese (non "Consuntivi"); Monitoraggio IoT (non "Cruscotti/ambienti"); Assistente AI (non "Agente/youworker.ai"). Render degli stati via label map (`in_revisione`→"In revisione", ecc.). Mai mostrare UUID `correlation` o `●●●` all'operatore.
- **10.7 Entry-point AI da ~10 a 2:** tenere gli `✦` inline (contestuali), unire i 3 globali in **"Assistente AI"** con toggle *Leggi/Agisci*, e "AI" sotto Impostazioni per i switch.

## 11. Top 10 cambi UI (impatto × effort)
| # | Cambio | Impatto | Effort |
|---|---|---|---|
| 1 | Eliminare il bottone morto "Chiedi ai documenti" (PWA) | Alto | Banale |
| 2 | Sostituire tutti i `prompt()`/`confirm()` nativi con askPrompt/askConfirm | Alto | Basso |
| 3 | ✅ FATTO (parz.) — Nav in sezioni titolate (Operatività/Lavoro/Configurazione) | Alto | Basso |
| 4 | Icona unica per ogni voce nav | Med-Alto | Banale |
| 5 | ✅ FATTO — Vista "I miei allarmi" per `reperibile` | Alto | Medio |
| 6 | Tab bar PWA + SOS sempre ancorato | Alto | Medio |
| 7 | Piantine dentro Mappa (tab) + mini-mappa nel drawer incidente | Alto | Medio |
| 8 | Drawer incidente: un primario, resto sotto "Altre azioni", niente UUID | Alto | Medio |
| 9 | Superadmin: tenant in lista ricercabile con dettaglio a tab, un solo confirm | Med-Alto | Medio |
| 10 | Code come default di ruolo (approvatore→"Da approvare", planner→"Da assegnare") + label map stati | Medio | Basso-Med |

Quick win 1-4,10 = modifiche di etichette/pattern con guadagno di chiarezza enorme → primi. 5-9 = mosse strutturali che risolvono il "operatori e field worker fanno fatica".
