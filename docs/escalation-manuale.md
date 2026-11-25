# Escalation e reperibilità — Manuale

Guida per l'**amministratore del tenant** (e per il superadmin, nella parte sui canali). Spiega
come un allarme raggiunge le persone, come si configura la reperibilità e cosa succede quando
nessuno risponde.

L'escalation è fatta **interamente da YouWorkforce** (motore nativo, unico motore). Il vecchio
motore esterno GoAlert/YWAlert è stato ritirato: non c'è nulla da collegare né da sincronizzare.
Il vecchio manuale è in [archivio/ywalert-manuale.md](archivio/ywalert-manuale.md), solo come
documento storico.

L'escalation non è solo di «Sicurezza lavoratore»: serve qualunque modulo che apre allarmi
(Lone Worker, Localizzazione GPS e badge, IoT, Ronde). Con uno qualunque di questi moduli attivo,
la pagina **Escalation e reperibilità** è disponibile.

Chi può fare cosa:
- configurare l'escalation: amministratore o permesso **«Escalation e reperibilità»**
  (`escalation.manage`);
- prendere in carico e chiudere gli allarmi: permesso **«Sicurezza · Gestire allarmi»**
  (operatore, responsabile, reperibile);
- impostare i canali SMS/chiamate/email/Slack: **solo il superadmin**.

---

## 1. Come un allarme arriva alle persone

1. Un dispositivo, l'app del lavoratore, un sensore o un controllo automatico (check-in scaduto,
   silenzio del badge, ronda mancata, monitor che non risponde…) apre un **allarme** in Centrale.
2. YouWorkforce decide **a quale gruppo** va, in quest'ordine:
   1. il gruppo scelto dal monitor che l'ha aperto (heartbeat o email → allarme, se impostato);
   2. il gruppo del **profilo di protezione** del lavoratore coinvolto (anche se usa un badge
      registrato a nome di un collega);
   3. il gruppo del **routing** per quel tipo di allarme (es. SOS → «Squadra emergenze»);
   4. il **gruppo di default** del tenant;
   5. se non c'è nessun gruppo e l'allarme è critico: la **rete di ultima istanza** (§9).
3. Del gruppo si calcola **chi è di turno adesso** (§3, §5).
4. Si seguono i **passi** dell'escalation del gruppo: chi avvisare, con che canale, quanto
   aspettare prima del passo successivo (§4).
5. La prima **presa in carico** (console, risposta SMS, tasto 1 al telefono) ferma tutto (§7).
6. Se nessuno risponde, l'allarme passa alla rete di ultima istanza e resta aperto in Centrale
   (§10).

Il motore controlla gli allarmi aperti **subito** quando ne nasce uno nuovo e poi circa **ogni
30 secondi**. Con molti allarmi aperti serve prima quelli a **priorità più alta** (un SOS non resta
in coda dietro allarmi meno urgenti), poi i più vecchi.

Un secondo segnale uguale mentre l'allarme è ancora aperto (es. il badge ripete l'SOS) non apre un
nuovo allarme: si aggiunge alla cronologia di quello esistente e l'escalation prosegue senza
raffiche di messaggi.

---

## 2. Prima configurazione

### 2.1 Canali di consegna (superadmin)

In **Superadmin → Sistema → «Canali di consegna degli allarmi»** il superadmin imposta:
- **Twilio** per SMS e chiamate: Account SID, Auth token, numero mittente (formato
  internazionale, es. `+39…`) e l'interruttore «Invio reale con Twilio»;
- **Slack**: token del bot;
- **Email**: indirizzo del relay (un servizio che inoltra i messaggi alla posta).

I token sono cifrati, non vengono più mostrati e non finiscono nel registro attività. I canali
sono uguali per tutti i tenant dell'installazione. Sono le uniche connessioni esterne di
YouWorkforce oltre all'AI: servono ad avvisare persone che non stanno guardando la console.

### 2.2 Cose da fare nel tenant (amministratore)

Menu **Sicurezza Lavoratore → Escalation e reperibilità**. Lista minima:
1. crea almeno un **gruppo** con i suoi membri (§3);
2. scegli il **gruppo di default**, che riceve gli allarmi senza un routing specifico;
3. compila la **rete di ultima istanza** (§9): senza, un allarme fuori copertura non arriva a
   nessuno, e la pagina lo segnala;
4. inserisci il **telefono** di ogni persona in *Persone e permessi* (formato `+393331234567`) e
   **verificalo** nella scheda *Recapiti*;
5. se servono, imposta il **routing** per tipo di allarme e i **profili di protezione** (menu
   *Regole di sicurezza*).

Le modifiche valgono **subito**.

La pagina ha sei schede: **Gruppi**, **Routing**, **Notifiche**, **Recapiti**, **Assenze & turni**,
**Monitor**.

---

## 3. Gruppi e turni di reperibilità

### 3.1 Gruppo e membri

Scheda **Gruppi** → **+ Nuovo gruppo** (es. «Reperibili notte»), poi **+ membro**. I membri si
possono anche importare da un **gruppo di persone** (*Persone e permessi*). L'ordine dei membri è
l'ordine della rotazione.

### 3.2 Chi è di turno

Per ogni gruppo, **Reperibilità**:
- **Tutti insieme**: ogni allarme avvisa tutti i membri contemporaneamente;
- **A turni (rotazione)**: una persona per volta. Tipo **orario, giornaliero, settimanale o
  mensile** e lunghezza del turno (es. 2 = turni di due giorni). Il turno mensile segue i mesi veri
  del calendario.

Nella sotto-scheda **⚙ Copertura · handoff · Slack**:
- **Copertura**: *24/7* oppure *solo fasce orarie* (dalle–alle, giorni della settimana). Fuori
  fascia il gruppo **non ha reperibili**: un allarme critico va alla rete di ultima istanza.
- **Handoff turno**: ora (e giorno, per i turni settimanali) del passaggio al turno successivo.
- Orari, fasce e passaggi seguono il **fuso orario del gruppo**, compresi i cambi d'ora legale.
- **Canale Slack** e **Avviso on-call Slack**: pubblica su Slack chi è di turno, a ogni cambio
  turno o ogni giorno alle 08:00 (serve il token Slack del superadmin).

**Attiva ora**: su un gruppo a turni, il pulsante accanto a una persona la mette **subito** di
turno, senza aspettare il passaggio (es. un collega dà il cambio in anticipo).

### 3.3 Assegnazioni aggiuntive

Sotto-scheda **👥 Assegnazioni**: coperture che si **aggiungono** a quella di base, ognuna con i
propri membri e una finestra (giorni + orario). Esempio: rotazione nei feriali + assegnazione
«Weekend» il sabato e la domenica + una persona fissa di notte. Un'assegnazione può avvisare tutti
i suoi membri insieme o farli ruotare.

### 3.4 Turni calcolati e calendario

- Sotto-scheda **📅 Turni**: il calendario **finale** dei prossimi 30 giorni (rotazione,
  copertura, assegnazioni, assenze e pianificazioni temporanee già applicate), in lista o griglia,
  filtrabile per persona. Qui c'è anche il **link iCal del gruppo**, che si può rinnovare (↻, il
  vecchio link smette di funzionare) o revocare.
- Scheda **Notifiche → Il mio calendario reperibilità**: link iCal personale (Google, Apple,
  Outlook) con i propri turni. Anche questo si rinnova o si revoca.

---

## 4. Passi dell'escalation e tempi

### 4.1 Senza configurare niente (sequenza predefinita)

Se il gruppo non ha passi personalizzati, vale questa sequenza verso chi è di turno:
1. **SMS subito**;
2. se nessuno prende in carico entro *N* minuti (**Notifiche → Fallback vocale**, predefinito 5,
   da 1 a 60), **chiamata**.

Con il fallback vocale spento c'è solo l'SMS.

### 4.2 Passi personalizzati (⛓ Policy)

Nella sotto-scheda **⛓ Policy** del gruppo si costruisce la sequenza (fino a 20 passi). Per ogni
passo:
- **canale**: SMS, chiamata, email o Slack;
- **chi avvisare**: chi è di turno nel gruppo (predefinito), **un altro gruppo** (es.
  «Responsabili»), **una persona** precisa, oppure **un canale Slack**;
- **attesa**: minuti (0–1440) da aspettare dopo questo passo prima di passare al successivo.

I passi si riordinano (↑ ↓). **Ripeti** (0–10) fa ripartire l'intera sequenza dall'inizio quante
volte si vuole, finché qualcuno non prende in carico.

Esempio di escalation gerarchica:

| Passo | Canale | Chi | Attesa |
|---|---|---|---|
| 1 | SMS | reperibile di turno | 5 min |
| 2 | chiamata | reperibile di turno | 5 min |
| 3 | SMS | gruppo «Responsabili» | 10 min |
| 4 | chiamata | direttore di stabilimento | 10 min |

Salvare una policy vuota riporta il gruppo alla sequenza predefinita.

### 4.3 Regole personali di ogni persona

Nella scheda **Recapiti** (⚙ su ogni persona) si aggiungono recapiti oltre al telefono principale
(altri numeri per SMS o chiamata, email) e le **regole di notifica**: quale recapito e **dopo
quanti minuti** dall'inizio del passo. Esempio: SMS subito, chiamata dopo 2 minuti.

Quando un passo avvisa una persona che ha regole personali, si seguono **le sue regole** al posto
del canale del passo. Senza regole si usa il canale del passo sul recapito di base (telefono, o
email per i passi email).

Le regole personali continuano anche quando l'escalation passa al passo successivo, finché
qualcuno non prende in carico: con «SMS subito, chiamata dopo 5 minuti» la chiamata parte a 5
minuti anche se il passo aspettava solo 2 minuti e nel frattempo è stato avvisato qualcun altro
(come in GoAlert). Con la presa in carico o la chiusura dell'allarme si fermano tutti gli avvisi.
Ogni avviso parte una volta sola. Prima del 26/11/2026 una regola ritardata scattava solo se il
passo era ancora in corso.

### 4.4 Avvisi di aggiornamento

**Notifiche → Avvisa sugli aggiornamenti**: chi è stato avvisato riceve un SMS quando un collega
prende in carico o chiude l'allarme. Ogni persona può sovrascrivere la scelta del tenant
(*eredita* / *attivo* / *disattivo*) nella scheda Recapiti.

---

## 5. Assenze, sostituzioni e pianificazioni temporanee

Scheda **Assenze & turni**:
- **Assenze e sostituzioni**: per un periodo (dal–al) e un gruppo, **togli** una persona e/o
  **metti** un sostituto. Le assenze si possono modificare o eliminare.
- **Pianificazione temporanea**: per un periodo definito sostituisce **del tutto** la reperibilità
  del gruppo con una sequenza di blocchi «persona + ore». Mentre è attiva riceve gli allarmi del
  gruppo **solo** la persona del blocco in corso; alla fine si torna ai turni normali.

L'effetto è immediato ed è visibile nei **Turni calcolati**.

---

## 6. Manutenzione

**Notifiche → Manutenzione**: 1, 2 o 4 ore, oppure **Termina** per chiuderla subito.

Durante la manutenzione:
- gli allarmi **non salva-vita** (sensori, dispositivi, monitor, aree, batteria…) si aprono e si
  vedono in Centrale, ma **non partono avvisi**;
- gli allarmi **salva-vita** (SOS, uomo a terra, immobilità, check-in mancato, perdita di
  contatto) **non si sospendono mai**: escalation e rete di ultima istanza continuano;
- a fine finestra chi è di turno nel **gruppo di default** riceve **un unico SMS di riepilogo**
  («N allarmi durante la manutenzione») e l'escalation degli allarmi ancora aperti riprende.

Senza gruppo di default il riepilogo non parte (gli allarmi riprendono comunque l'escalation).

---

## 7. Presa in carico

La presa in carico **ferma l'escalation** di quell'allarme. Si fa in tre modi.

**Dalla console** — *Centrale allarmi* (o *Assegnati a me* per reperibile e responsabile):
**Prendi in carico**, poi **Assegna**, **Nota**, **Chiudi** con l'esito. In Centrale si vede anche
chi è di turno in questo momento.

**Rispondendo all'SMS** — il messaggio dice di cosa si tratta e come rispondere, ad esempio:
«ALLARME Uomo a terra - mario@… - Capannone A - posizione https://… . Rispondi OK 1a2b3c4d per
prenderlo in carico, CHIUDI 1a2b3c4d per chiuderlo.»
- la parola deve essere la **prima** del messaggio: `OK`, `ACK`, `1` o `PRESO` = presa in carico;
  `CHIUDI` o `RISOLTO` = chiusura (quindi «NON OK» non vale);
- il **codice** dopo la parola indica l'allarme. Senza codice la risposta vale solo se quel numero
  ha un unico allarme da gestire, altrimenti arriva un SMS con l'elenco dei codici;
- valgono solo gli allarmi per cui **quel numero è stato avvisato**.

**Rispondendo alla chiamata** — la voce legge l'allarme: **1** = presa in carico, **9** = non
ricevere più chiamate (opt-out).

**Opt-out.** Chi risponde **STOP** a un SMS o preme **9** al telefono non riceve più nulla su quel
recapito (vale ovunque quel numero compaia, anche nella rete di ultima istanza). Per riattivarlo:
la persona risponde **START**, oppure l'amministratore preme **Riabilita** nella scheda Recapiti;
poi il recapito va **verificato di nuovo**. Lo stato di opt-out è segnato in rosso.

**Verifica dei recapiti** (scheda Recapiti): **invia codice** (arriva un SMS con un codice valido
10 minuti), **inserisci codice**, **prova** (SMS di test). La verifica conferma che il numero è
giusto; gli avvisi partono comunque anche verso numeri non ancora verificati, quindi verificare è
l'unico modo per scoprire un numero sbagliato prima di un'emergenza.

---

## 8. Consegna reale, di prova o simulata

Lo stato dipende dai canali impostati dal superadmin (§2.1):

| Stato | Cosa significa |
|---|---|
| **reale** | SMS e chiamate partono davvero verso i telefoni (Twilio configurato e «Invio reale» acceso) |
| **di prova** | gli avvisi vanno a un ricevitore di test dell'installazione, non ai telefoni |
| **simulata** | nessun canale: **nessun reperibile viene avvisato**, gli avvisi restano solo nei log |

Email e Slack sono indicati a parte (reale se il relay o il token sono impostati).

Se la consegna non è reale, chi segue gli allarmi vede in Centrale, in *Assegnati a me* e in
Escalation l'avviso **«Gli allarmi non arrivano ai reperibili»** (o «di prova»): va avvisato subito
il superadmin. La Centrale continua comunque a mostrare tutti gli allarmi. Il superadmin vede lo
stesso controllo nella scheda *Sistema* (giallo = di prova, rosso = simulata).

---

## 9. Rete di ultima istanza

Riquadro **Rete di ultima istanza** nella scheda Gruppi: un elenco di contatti **email e/o SMS**
dell'azienda (fino a 50, con nome facoltativo), per esempio la portineria, il responsabile della
sicurezza, un istituto di vigilanza.

Interviene **solo per gli allarmi critici** quando:
- un passo non ha destinatari (fuori dalla fascia di copertura, gruppo vuoto, nessun recapito):
  in cronologia compare «nessun destinatario per questo gradino»;
- l'allarme non ha alcun gruppo (né monitor, né profilo, né routing, né default);
- tutti i passi (ripetizioni comprese) sono finiti e nessuno ha preso in carico.

Modalità **Tutti insieme** oppure **Uno dopo l'altro** (intervallo da 30 secondi a un'ora,
predefinito 2 minuti). **La prima presa in carico ferma la catena**: i contatti non ancora avvisati
non lo saranno. Messaggio: «ALLARME <tipo> senza reperibile disponibile – <sede> (incidente
<codice>). Rispondi OK per prenderlo in carico.»

Se la lista è vuota la pagina avvisa che questi allarmi non arrivano a nessuno: **configurarla è
necessario**.

---

## 10. Quando nessuno risponde

In ordine:
1. i passi della policy proseguono uno dopo l'altro con le loro attese, poi la sequenza si ripete
   tante volte quante indicato in **Ripeti**;
2. finiti i passi, per un allarme critico parte la **rete di ultima istanza**;
3. l'allarme **resta aperto** in Centrale, lampeggiante, finché qualcuno non lo prende in carico o
   lo chiude: non si chiude mai da solo per mancata risposta;
4. tutto resta nella **cronologia** dell'allarme: ogni invio, gli invii falliti, i passi senza
   destinatari, l'avvio della rete e il motivo.

Un invio fallito (es. il provider non risponde) viene **ritentato** al giro successivo: un SOS non
si perde per un errore temporaneo.

Se anche la rete è vuota, l'allarme è visibile solo in console: per questo la rete va configurata
e la consegna deve essere **reale**.

---

## 11. Altri strumenti del gruppo e del tenant

- **🏷 Label**: etichette `chiave=valore` (es. `turno=notte`) per filtrare e ritrovare i gruppi.
- **📋 Playbook**: le istruzioni «cosa fare» quando scatta un allarme del gruppo, e le **note di
  passaggio turno** lasciate da chi smonta per chi entra.
- **Scheda Monitor → Heartbeat monitor**: un sistema esterno chiama periodicamente un indirizzo
  generato da YouWorkforce; se smette oltre il tempo impostato (predefinito 15 minuti) si apre un
  allarme «monitor non risponde», instradato al gruppo scelto; si chiude da solo quando le chiamate
  riprendono.
- **Scheda Monitor → Email → allarme**: un indirizzo email del tenant; un messaggio ricevuto lì
  diventa un allarme instradato. Più email con lo stesso `indirizzo+chiave@dominio` confluiscono
  nello stesso allarme aperto.
- **Scheda Monitor → Diagnostica**: **Report consegne** (quanti avvisi, in che stato, gli ultimi
  invii).
- **Chiudi tutti** (Centrale): chiude gli allarmi aperti con un unico esito, **tranne i
  salva-vita**; per chiudere anche quelli serve una seconda conferma («ho verificato che ogni
  persona è al sicuro»).

---

## 12. Risoluzione problemi

| Sintomo | Causa probabile | Rimedio |
|---|---|---|
| Avviso «Gli allarmi non arrivano ai reperibili» | consegna simulata o di prova | il superadmin configura Twilio e accende l'invio reale |
| Una persona non viene avvisata | telefono mancante o sbagliato, opt-out, fuori turno | telefono in *Persone e permessi*, poi **verifica**; controlla *Turni calcolati* e lo stato opt-out |
| Niente chiamata dopo l'SMS | fallback vocale spento, o policy senza passo «chiamata» | attiva il fallback vocale o aggiungi un passo chiamata |
| Una regola personale «chiamata dopo N minuti» non parte | l'attesa del passo è più breve di N | aumenta l'attesa del passo (§4.3) |
| Allarme critico senza destinatari | nessun routing né gruppo di default, o fuori copertura | imposta il gruppo di default e la rete di ultima istanza |
| Allarmi di sensori/dispositivi non inoltrati | manutenzione attiva | **Termina** la manutenzione (i salva-vita passano comunque) |
| La pagina Escalation non compare | nessun modulo che apre allarmi è attivo, o manca il permesso | attiva Lone Worker, GPS, IoT o Ronde; permesso «Escalation e reperibilità» |

---

## Sezione tecnica

### Dove sta il codice

- `core/src/oncall.rs` — impostazioni, recapiti e verifica, regole di notifica, assenze,
  pianificazioni temporanee, manutenzione, calendari iCal, calcolo di chi è di turno
  (`esc_native_oncall_contacts`), policy dei gruppi (`esc_group_policy`), runner di consegna
  (`escalation_delivery_run_tenant`, `esc_process_incident`), rete di ultima istanza
  (`esc_fallback_*`), risposte Twilio SMS/voce, heartbeat ed email → allarme.
- `core/src/escalation.rs` — parte pura: rotazioni con fuso e ora legale, macchina a stati dei
  passi (`tick`: `FireStep` / `Wait` / `Stop` / `Exhausted`).
- `core/src/delivery.rs` — adattatori di consegna (Twilio REST, Slack `chat.postMessage`, relay
  email, ricevitore di prova, modalità «solo log»), canali configurabili dal superadmin
  (`CHANNEL_KEYS`, `chan`), stato di consegna (`delivery_status`), verifica della firma Twilio.
- `core/src/worker.rs` — il worker lancia il runner a ogni allarme nuovo (outbox `notify`) e con la
  scansione periodica (`escalation_delivery_scan`, circa ogni 30 s, un tenant per volta).

### Instradamento

Gruppo di un incidente: `COALESCE(forced_group_id, profilo di protezione attivo del lavoratore,
alarm_routing per etype, tenant_oncall_cfg.default_group_id)`. Il lavoratore è
`incidents.worker_user_id` oppure l'assegnatario del dispositivo dell'evento. Tipi critici
(`CRITICAL` in `api.rs`): sos, man_down, immobilita, mancato_checkin, sensore_critico,
manomissione_badge, perdita_alimentazione, jamming, perdita_contatto, batteria_critica,
heartbeat_down, email_alarm, geofence_ingresso, geofence_uscita, ronda_mancata. Salva-vita
(`LIFE_SAFETY`): sos, man_down, immobilita, mancato_checkin, perdita_contatto.

### Macchina a stati e idempotenza

- Stato per incidente in `app.esc_alert_state` (passo corrente, ora di scatto, giri di ripetizione,
  stato `unack` / `ack` / `closed` / `exhausted`). Presa in carico (`preso_in_carico`,
  `intervento`) e chiusura fermano la sequenza.
- Passi in `app.esc_policy_steps` (`delay_s`, `channel`, `target_kind` oncall/group/user/slack);
  ripetizione in `alarm_groups.esc_repeat` (0–10). Senza passi: sequenza derivata da
  `tenant_oncall_cfg.voice_fallback_on/voice_fallback_min`.
- Ogni invio è registrato in `app.esc_delivery_log` con chiave unica (incidente, passo, canale,
  destinatario, giro): lo stesso avviso non parte due volte, neanche tra riavvii o più istanze. Un
  invio fallito libera la chiave e viene ritentato al giro dopo (almeno una volta).
- Lo scan legge solo le escalation **attive** (esclude `exhausted`, `ack`, `closed`), ordinate per
  priorità decrescente e apertura; pagina `ESC_INCIDENTS_PER_TICK` (1000) per tenant e giro.
- Rete di ultima istanza: `app.esc_fallback_contacts`, `app.esc_fallback_state`, configurazione
  in `tenant_oncall_cfg.fallback_mode/fallback_interval_s` (mig 0131); passo fittizio `-1` nel log.
- Manutenzione: `tenant_oncall_cfg.maintenance_until` (API 0–24 h, console 1/2/4 h); gli incidenti
  non salva-vita vengono marcati `bundled_at` e riepilogati all'uscita.
- Tenant sospeso: nessuna consegna; riprende alla riattivazione per gli allarmi ancora aperti.

### Canali e variabili

| Chiave | Uso |
|---|---|
| `YW_ESC_TWILIO_SID`, `YW_ESC_TWILIO_AUTH`, `YW_ESC_FROM` | credenziali e mittente Twilio (superadmin, tabella `app.delivery_channels`, mig 0152; l'ambiente vale solo come valore iniziale) |
| `YW_ESC_TWILIO_LIVE` | `1` = invio reale (usato da `delivery_status`) |
| `YW_ESC_SLACK_TOKEN` | Slack |
| `YW_ESC_SMTP_RELAY_URL` | relay email |
| `YW_ESC_SINK_URL` | ricevitore di prova (solo installazione, usato dagli e2e) |
| `YW_ESC_PUBLIC_BASE` | indirizzo pubblico per l'URL di risposta della chiamata e degli heartbeat |

Risposte in ingresso: `POST /api/ingress/twilio/sms` e `/api/ingress/twilio/voice`, con verifica
della firma `X-Twilio-Signature`.

### API principali (permesso `escalation.manage` salvo dove indicato)

| Area | Rotte |
|---|---|
| gruppi e routing | `GET/POST /api/groups`, `DELETE /api/groups/:id`, `/api/groups/:id/members[/import]`, `POST /api/groups/:id/oncall`, `POST /api/groups/:id/rotation/set-active`, `POST /api/routing` |
| policy | `GET/POST /api/groups/:id/policy` (`{steps:[{delay_min,channel,target_kind,…}], repeat}`) |
| turni | `GET /api/groups/:id/shifts`, `/api/groups/:id/assignments`, `/api/groups/:id/calendar[/url]` |
| impostazioni | `GET/POST /api/oncall-settings` (fallback vocale, aggiornamenti, gruppo di default) |
| recapiti | `/api/oncall-contacts[/verify|/code|/test]`, `/api/oncall-contacts/:uid/methods`, `/rules`, `/reactivate`, `/status-pref` |
| assenze | `/api/oncall-overrides[/update|/delete]`, `/api/oncall-temp-schedules[/delete]` |
| manutenzione | `POST /api/oncall-maintenance` (`{hours}`, 0 = termina) |
| rete di ultima istanza | `GET/PUT /api/escalation/fallback` |
| diagnostica | `GET /api/escalation/delivery`, `POST /api/escalation/delivery/run` |
| stato consegna | `GET /api/delivery-status` (chi segue gli allarmi) |
| calendario personale | `GET /api/oncall-calendar/url`, `DELETE /api/oncall-calendar` (utente) |

### Robustezza del runner

- Stack dei thread del worker: `YW_WORKER_STACK_MB` (default **32**). Il runner elabora un tenant
  per volta su un task dedicato e ogni incidente con `Box::pin` (stack costante qualunque sia il
  numero di allarmi aperti); le chiamate HTTP verso i provider sono anch'esse su heap.
- Il set di lavoro contiene solo le escalation attive, quindi resta piccolo; se in un'emergenza di
  massa supera la pagina, il resto passa al giro dopo con priorità ai più urgenti.
- Coda anomala visibile: `system_snapshot` espone `queues.esc_active_max` e il watchdog apre
  l'alert di sistema `esc_backlog` (`YW_SYS_ESC_BACKLOG_WARN` 500 / `YW_SYS_ESC_BACKLOG_BAD`
  2000), notificato al superadmin.

### Prove

`qa/escalation-fallback-e2e.sh`, `qa/sms-ack-e2e.sh`, `qa/delivery-status-e2e.sh`,
`qa/delivery-channels-e2e.sh`, `qa/miei-allarmi-e2e.sh` e le suite di escalation del gate
`MODE=core` (`qa/run-all.sh`).
