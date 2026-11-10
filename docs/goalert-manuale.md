# Reperibilità e notifiche (GoAlert) — Manuale

Guida per l'amministratore YouWorkforce. Spiega **come configurare** e **come usare** la
reperibilità. Non serve mai aprire la console GoAlert: tutto si fa da YouWorkforce.

GoAlert è il motore di escalation integrato: YouWorkforce gli dice "è successo un allarme",
lui avvisa la persona giusta nel modo giusto (SMS, chiamata, email, Slack) e continua a
insistere finché qualcuno prende in carico.

---

## Parte 1 — Configurazione (una volta)

### 1.1 Attivare il motore (superadmin)
1. Pannello **Superadmin → tenant → Connessioni & integrazioni**.
2. **Provisiona GoAlert**: crea per il tenant l'escalation policy, il servizio, la chiave e
   il heartbeat. Di norma avviene **in automatico**; il semaforo segnala eventuali anomalie.

### 1.2 Scegliere il trasporto (superadmin → Trasporto notifiche)
Il trasporto decide *come* arrivano gli avvisi. Imposta almeno uno:
- **Twilio** (SMS + voce): `Account SID`, `Auth Token`, `From Number` (E.164, es. `+39…`).
  Necessario per SMS e per il **fallback vocale**.
- **SMTP** (email): server, utente, password, mittente.
- **Slack** (opzionale): per avvisi su canale.

> Senza alcun trasporto attivo, GoAlert non può consegnare: YouWorkforce lo segnala in rosso
> dopo la sincronizzazione.

---

## Parte 2 — Uso quotidiano (admin del tenant)

Tutto è in **Gruppi allarme**. Il principio: configuri qui, poi premi **🔄 Sincronizza con
GoAlert** per applicare. Un solo pulsante.

### 2.1 Gruppi, membri, routing
1. **+ Nuovo gruppo** (es. "Reperibili notte").
2. Aggiungi i **membri** (persone con ruolo reperibile/operatore).
3. **Routing eventi → gruppo**: per ogni tipo di allarme (SOS, uomo a terra, …) scegli il
   gruppo responsabile.

### 2.2 Reperibilità del gruppo
- **Tutti insieme**: tutti i membri avvisati contemporaneamente.
- **A turni (rotazione)**: a turno, una persona per volta. Scegli tipo (orario/giorn./settim./
  mensile) e durata del turno.

### 2.3 Copertura · handoff · Slack (menu ⚙ sul gruppo)
- **Copertura**:
  - *24/7*: sempre attivo.
  - *Solo fasce orarie*: attivo solo in una finestra (es. 20:00–08:00) e nei giorni scelti.
    Fuori fascia gli allarmi vengono registrati ma non inoltrati a questo gruppo.
- **Handoff turno** (solo a turni): ora e giorno in cui il turno passa alla persona successiva
  (es. ogni lunedì alle 09:00).
- **Canale Slack**: ID del canale per gli avvisi.
- **Avviso on-call Slack**: *off* / *a ogni cambio turno* / *ogni giorno 08:00* — pubblica su
  Slack chi è di turno.

### 2.4 Reperibilità avanzata (sezione in fondo)

**Notifiche**
- **Fallback vocale**: se l'SMS non riceve risposta entro *N minuti*, parte una **chiamata**.
  Fortemente consigliato per la sicurezza. Imposta i minuti (default 5).
- **Avvisa sugli aggiornamenti**: il reperibile riceve notifica se qualcun altro modifica
  l'allarme.

**Manutenzione** — sospende l'escalation per 1/2/4 ore (durante lavori programmati). Gli
allarmi vengono comunque registrati, ma non si disturba nessuno. **Termina** la riattiva.

**Il mio calendario reperibilità** — genera un URL iCal da incollare in Apple/Google Calendar:
vedi i tuoi turni nel calendario personale (aggiornamento automatico).

**Recapiti dei reperibili (verifica)** — mostra per ogni persona i recapiti registrati in
GoAlert (SMS, voce, email) e il loro stato:
- **invia codice**: GoAlert manda un codice di verifica al recapito.
- **inserisci codice**: confermi il codice ricevuto → recapito ✓ verificato.
- **prova**: invia un messaggio di test.

Un recapito **non verificato** potrebbe non ricevere: verifica sempre SMS e voce.

**Assenze e sostituzioni** — per un periodo (ferie, malattia) puoi:
- **togliere** una persona dalla reperibilità, e/o
- **mettere** un sostituto al suo posto.
Scegli il gruppo, le persone e le date. Si applica subito, senza toccare i turni base.

### 2.5 Applicare — Sincronizza con GoAlert
Premi **🔄 Sincronizza con GoAlert**. YouWorkforce allinea in GoAlert: utenti, recapiti
(SMS + voce con fallback), escalation policy, rotazioni con handoff, pianificazioni con
copertura oraria, avvisi Slack ed etichette. L'esito mostra quanti gruppi/utenti/recapiti e
lo stato del trasporto. Un avviso rosso = nessun trasporto attivo (nessuno verrebbe avvisato).

---

## Parte 3 — Cosa succede in automatico

- **Catena allarme**: allarme → YouWorkforce apre l'alert in GoAlert → SMS immediato →
  (se nessuno risponde) chiamata → ripetizione secondo l'escalation SOS.
- **Ack/chiusura bidirezionale**: se il reperibile prende in carico o chiude l'allarme da
  SMS, telefono o console GoAlert, **l'incidente in YouWorkforce si aggiorna da solo**
  (preso in carico / chiuso), con riga in timeline.
- **Rientro**: quando la condizione rientra (es. check-in ripristinato), YouWorkforce chiude
  l'alert in GoAlert.
- **Heartbeat**: se la pipeline di YouWorkforce smette di contattare GoAlert, GoAlert genera
  un allarme di servizio (ci accorgiamo subito di un'interruzione).
- **Etichette**: ogni servizio GoAlert è etichettato `youworkforce/tenant` e
  `youworkforce/group` (ritrovabilità).

---

## Parte 4 — Risoluzione problemi

| Sintomo | Causa probabile | Rimedio |
|---|---|---|
| "Nessun trasporto attivo" dopo la sync | Twilio/SMTP non configurati | Superadmin → Trasporto notifiche |
| Membro non avvisato | manca telefono/email, o recapito non verificato | aggiungi recapito in Utenti e sedi; poi **verifica** |
| Niente chiamata di fallback | fallback vocale off, o Twilio non attivo | attiva fallback vocale + Twilio |
| "GoAlert non provisionato" | tenant non provisionato | Superadmin → Provisiona GoAlert |
| Assenze non applicabili | manca la pianificazione | premi **Sincronizza con GoAlert** (crea la schedule) |
| Allarmi durante manutenzione | manutenzione attiva | **Termina** la manutenzione |

---

## Riepilogo campi

- **Copertura**: 24/7 oppure fasce (dalle–alle + giorni).
- **Handoff**: ora + giorno di passaggio turno.
- **Fallback vocale**: on/off + minuti (1–60).
- **Avvisa sugli aggiornamenti**: on/off.
- **Canale Slack** + **Avviso on-call**: off / a ogni cambio / orario fisso.
- **Assenze**: gruppo + assente e/o sostituto + dal–al.
- **Manutenzione**: 1h / 2h / 4h / termina.
