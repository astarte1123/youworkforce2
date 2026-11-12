# Reperibilità e notifiche — Manuale

Guida per l'**amministratore YouWorkforce**. Spiega **come configurare** e **come usare** la
reperibilità e l'escalation degli allarmi. Tutto si fa dentro YouWorkforce: non serve aprire
nessuna console esterna.

Il **motore di escalation è nativo** di YouWorkforce ed è quello usato di default: quando succede
un allarme, avvisa la persona giusta nel modo giusto (SMS, chiamata, email, Slack) e continua a
insistere finché qualcuno prende in carico. **GoAlert resta come rete di sicurezza / rollback**
(si può riattivare dal superadmin), ma non è più necessario.

> L'escalation **non appartiene solo a "Sicurezza lavoratore"**: serve qualunque modulo che
> produce allarmi (IoT, ThingsBoard, Traccar). Anche con "Sicurezza lavoratore" spento, la Centrale
> allarmi e la configurazione dell'escalation restano disponibili per instradare gli allarmi degli
> altri moduli.

---

## Parte 1 — Configurazione (una volta)

### 1.1 Trasporto notifiche (superadmin → Trasporto notifiche / env)
Il trasporto decide *come* arrivano gli avvisi:
- **Twilio** (SMS + voce): account SID, auth token, numero mittente (E.164, es. `+39…`).
  Necessario per SMS reali e per il **fallback vocale**.
- **Email** (relay HTTP): per l'invio via email.
- **Slack** (opzionale): token per avvisi su canale.

In assenza di trasporto reale (ambiente di prova), gli invii vengono simulati (nessun messaggio
esce davvero): la logica di escalation funziona comunque e resta testabile.

### 1.2 Motore di escalation (superadmin → scheda tenant)
Nel pannello superadmin, per ogni tenant, **Motore escalation**:
- **nativo** (default, consigliato): il motore di YouWorkforce consegna e gestisce ack/chiusura.
- **GoAlert** (rollback): rimette GoAlert come motore. Da usare solo per tornare indietro.

---

## Parte 2 — Uso quotidiano (admin del tenant)

Tutto è in **Escalation e reperibilità** (gruppo menu *Sicurezza Lavoratore*; la voce resta visibile
anche se è attivo solo un modulo IoT/ThingsBoard/Traccar). Le modifiche native hanno effetto
**subito**, senza passaggi di sincronizzazione.

### 2.1 Gruppi, membri, routing
1. **+ Nuovo gruppo** (es. "Reperibili notte").
2. Aggiungi i **membri** (persone con ruolo reperibile/operatore).
3. **Routing eventi → gruppo**: per ogni tipo di allarme (SOS, uomo a terra, …) scegli il gruppo
   responsabile.
4. **Gruppo di default (catch-all)**: nella sezione impostazioni, scegli un gruppo che riceve gli
   allarmi non coperti da un routing specifico. Garantisce che nessun allarme critico resti senza
   destinatari.

Precedenza di instradamento di un incidente: **profilo di protezione del lavoratore → routing
per-tipo → gruppo di default**.

### 2.2 Reperibilità del gruppo
- **Tutti insieme**: tutti i membri avvisati contemporaneamente.
- **A turni (rotazione)**: a turno una persona per volta. Tipo **orario / giornaliero / settimanale
  / mensile** e durata del turno. La rotazione mensile rispetta il calendario (giorni reali del
  mese, nessuna approssimazione).
- **Fuso orario**: handoff, fasce di copertura e annunci Slack orari sono calcolati nell'**ora locale**
  del gruppo (campo fuso), con i cambi d'ora (DST) gestiti correttamente.
- **Attiva ora** (*Set Active*): su un gruppo a turni, il pulsante **"attiva ora"** accanto a una
  persona la rende **subito** reperibile di turno, senza aspettare il prossimo handoff (es. un collega
  dà il cambio in anticipo o rientra da un imprevisto).

### 2.2b Assegnazioni aggiuntive e Turni calcolati (sulla scheda gruppo)
- **Assegnazioni aggiuntive (coperture)**: oltre alla rotazione/copertura base del gruppo, puoi
  aggiungere N **assegnazioni**, ognuna con i propri membri e una **finestra** (giorni + orario, ora
  locale). Quando la finestra è attiva, quei membri si **aggiungono** all'on-call. Esempio: rotazione
  base nei feriali + assegnazione "Weekend" (sab/dom) + persona fissa di notte. Vuoto = 24h.
- **Turni calcolati (30 giorni)**: mostra il **calendario finale** del gruppo — chi è reperibile e
  quando nei prossimi 30 giorni, applicando rotazione, copertura, override, assegnazioni e
  pianificazioni temporanee.

### 2.3 Copertura · handoff · Slack (menu ⚙ sul gruppo)
- **Copertura**: *24/7* oppure *solo fasce orarie* (es. 20:00–08:00) nei giorni scelti. Fuori fascia
  gli allarmi vengono registrati ma non inoltrati a questo gruppo.
- **Handoff turno** (a turni): ora e giorno di passaggio al turno successivo.
- **Canale Slack** + **Avviso on-call Slack**: *off* / *a ogni cambio turno* / *orario giornaliero*
  — pubblica su Slack chi è di turno (richiede il token Slack configurato).

### 2.4 Policy di escalation multi-step (⛓ sulla scheda gruppo)
Definisci una **sequenza di step** personalizzata (parità con i motori professionali):
- per ogni step: **canale** (SMS / Voce / Email / Slack), **target** (chi notificare) e **attesa** in
  minuti prima di passare allo step successivo;
- il **target** dello step può essere: *on-call del gruppo* (default), *un altro gruppo* (es.
  Responsabili), *una persona* specifica, o *un canale Slack*. Così puoi fare **escalation gerarchica**
  (step1 = reperibile, step2 = responsabili, step3 = manager);
- **riordina** gli step (↑ ↓), aggiungi/rimuovi;
- **ripeti** l'intera sequenza N volte finché non c'è presa in carico.

Ogni destinatario viene comunque espanso nelle **sue regole di notifica** (recapiti + ritardi).
Se non definisci step personalizzati, vale la **ladder derivata** dalle impostazioni vocali
(SMS immediato → chiamata dopo i minuti di fallback) verso l'on-call del gruppo.

### 2.5 Label (ricerca)
Assegna a un gruppo **etichette** `chiave=valore` (es. `env=prod`, `team=ops`). Servono a filtrare e
ritrovare i gruppi (ricerca per chiave/valore o testo nel nome).

### 2.5b Playbook e note di passaggio (sulla scheda gruppo)
- **Playbook**: testo "cosa fare" quando scatta un allarme del gruppo (es. *1) verifica sede 2) chiama
  responsabile*). Resta a disposizione del reperibile.
- **Note di passaggio turno**: a fine turno chi smonta lascia una nota (eventi rilevanti, anomalie in
  corso) per il turno entrante.

### 2.5c Heartbeat monitor e ingress email (sezione avanzata)
- **Heartbeat monitor**: un sistema esterno (un nodo, una pipeline) chiama periodicamente l'URL
  generato. Se smette di chiamare oltre il **timeout** (default 15 min), si apre un allarme
  `heartbeat_down` instradato al gruppo scelto (o al routing/default). Alla ripresa del ping l'allarme
  si chiude da solo. *NB deploy: l'URL è assoluto solo se è configurato `YW_ESC_PUBLIC_BASE`.*
- **Ingress email → allarme**: genera un **indirizzo** per-tenant; un'email ricevuta a quell'indirizzo
  (via webhook del provider di posta) diventa un allarme `email_alarm` instradato. Dedup: usa
  `indirizzo+chiave@dominio` per far confluire più email nello stesso allarme aperto.

### 2.6 Reperibilità avanzata (sezione in fondo)

**Notifiche**
- **Fallback vocale**: se l'SMS non riceve risposta entro *N minuti*, parte una **chiamata**
  (consigliato; default 5).
- **Avvisa sugli aggiornamenti**: il reperibile riceve notifica se qualcun altro modifica l'allarme.

**Recapiti dei reperibili (verifica)** — per ogni persona il telefono e lo stato di verifica:
- **invia codice**: YouWorkforce invia via SMS un codice di verifica (OTP).
- **inserisci codice**: confermi il codice ricevuto → recapito ✓ verificato.
- **prova**: invia un SMS di test.

**Recapiti aggiuntivi e regole di notifica** (⚙ su ogni persona) — oltre al telefono principale puoi
aggiungere **più recapiti** (SMS / voce / email) e costruire la **ladder personale** della persona:
ogni *regola* indica quale recapito contattare e **dopo quanti minuti**. Esempio consigliato: SMS
immediato + chiamata dopo 2 minuti. Quando la policy del gruppo notifica quella persona, si seguono
le sue regole; senza regole si usa il recapito di base. Ogni recapito aggiuntivo si **verifica** con
un codice (invia codice → inserisci codice) e si può **provare**.

**Opt-out / Do-Not-Call (compliance)** — un reperibile può disattivare le notifiche rispondendo
**STOP** a un SMS, o premendo **9** durante una chiamata. Da quel momento YouWorkforce **non consegna
più** a quel recapito (vale ovunque quel numero compaia). Per riattivare: la persona risponde **START**
via SMS, oppure l'admin preme **Riabilita** sul recapito; poi il recapito va **ri-verificato**. Lo stato
opt-out è segnalato in rosso nei recapiti e **persiste** anche se il modulo viene spento e riacceso.

**Aggiornamenti allarme (per persona)** — oltre al default del tenant, ogni reperibile può scegliere se
ricevere un avviso quando un *altro* collega prende in carico o chiude un allarme di cui è on-call
(*eredita tenant* / *attivo* / *disattivo*).

**Assenze e sostituzioni** — per un periodo puoi **togliere** una persona e/o **mettere** un
sostituto, scegliendo gruppo, persone e date. Si applica subito.

**Pianificazione temporanea (sostituzione a tempo)** — per un periodo definito sostituisci del tutto
la reperibilità del gruppo con una sequenza di **blocchi** (persona + ore). Mentre è attiva, **solo**
la persona del blocco corrente riceve gli allarmi del gruppo. Alla fine si torna ai turni normali.

**Manutenzione** — sospende l'escalation per 1/2/4 ore (lavori programmati). Gli allarmi vengono
comunque registrati; a **fine finestra** gli on-call ricevono **un unico riepilogo** degli allarmi
accumulati, poi l'escalation riprende. **Termina** la riattiva subito.

**Il mio calendario reperibilità** — genera un URL iCal (Apple/Google Calendar) con i tuoi turni,
**calcolati dalla rotazione** dei prossimi 30 giorni oltre agli eventuali turni manuali.

**Diagnostica motore** — mostra il motore corrente, il **report delle consegne** (quante/stato) e il
**report di parità** (confronto nativo vs GoAlert quando GoAlert è presente).

---

## Parte 3 — Cosa succede in automatico

- **Catena allarme**: allarme → incidente in Centrale → escalation secondo la policy del gruppo
  (step/canali/attese/repeat) espansa nelle regole di notifica di ogni persona.
- **Dedup**: un secondo allarme con la **stessa chiave** (correlation) di un incidente ancora aperto
  non ne apre un altro (viene annotato sull'esistente).
- **Ack/chiusura**: se il reperibile prende in carico o chiude via SMS (rispondendo `OK`/`CHIUDI`),
  tastierino sulla chiamata (**1** = presa in carico, **9** = opt-out), o dalla Centrale, l'incidente
  si aggiorna (preso in carico / chiuso).
- **Opt-out rispettato**: un recapito che ha fatto STOP/9 non riceve più nulla finché non viene
  riattivato (START o Riabilita + ri-verifica).
- **Rientro**: quando la condizione rientra, l'incidente si chiude.
- **Self-sufficient**: il motore nativo funziona **senza GoAlert**. Se GoAlert è provvisto, viene
  usato solo come rete di sicurezza e per il confronto di parità.

---

## Parte 4 — Risoluzione problemi

| Sintomo | Causa probabile | Rimedio |
|---|---|---|
| Nessun avviso consegnato (ma incidenti aperti) | nessun trasporto reale (ambiente di prova) | configura Twilio/Email/Slack |
| Membro non avvisato | manca telefono, o recapito non verificato | aggiungi il telefono in Utenti; poi **verifica** |
| Niente chiamata di fallback | fallback vocale off, o Twilio non attivo | attiva fallback vocale + Twilio |
| Allarme critico senza destinatari | nessun routing né gruppo di default | imposta il **gruppo di default** o un routing |
| Allarmi durante manutenzione | manutenzione attiva | **Termina** la manutenzione |
| La sezione escalation non compare | nessun modulo che produce allarmi è attivo | attiva almeno uno fra Sicurezza lavoratore / IoT / ThingsBoard / Traccar |

---

## Riepilogo campi

- **Motore**: nativo (default) / GoAlert (rollback, superadmin).
- **Reperibilità**: tutti insieme / a turni (orario·giorn.·settim.·mensile + durata).
- **Copertura**: 24/7 oppure fasce (dalle–alle + giorni). **Handoff**: ora + giorno.
- **Policy step**: canale (SMS/Voce/Email/Slack) + attesa minuti, riordino, **ripeti N**.
- **Recapiti**: telefono principale (verifica OTP) + recapiti aggiuntivi + **regole** (recapito + ritardo).
- **Label**: coppie `chiave=valore` (ricerca).
- **Assenze**: gruppo + assente e/o sostituto + dal–al.
- **Pianificazione temporanea**: dal–al + blocchi (persona + ore).
- **Manutenzione**: 1h / 2h / 4h / termina (con riepilogo a fine finestra).
- **Fallback vocale**: on/off + minuti (1–60). **Avvisa sugli aggiornamenti**: on/off.
- **Avviso on-call Slack**: off / a ogni cambio / orario fisso.
