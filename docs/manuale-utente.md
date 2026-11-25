# YouWorkforce — Manuale utente

Questo è il manuale unico di YouWorkforce (YW) per chi lo usa ogni giorno: amministratori
dell'azienda, operatori della centrale allarmi, reperibili, responsabili, pianificatori, lavoratori
sul campo e auditor. L'ultima parte è per il superadmin della piattaforma. In fondo c'è una breve
appendice tecnica per chi installa e gestisce il server.

Aggiornato al 25/11/2026. I nomi di pagine e menu sono quelli del menu attuale (27 voci con schede).

> Per gli sviluppatori il registro dettagliato delle modifiche resta [manuale.md](manuale.md);
> i manuali di area sono elencati in [Per saperne di più](#per-saperne-di-piu).

---

## Indice

1. [Cos'è YouWorkforce e come è organizzato](#cap1)
   - [Le due grandi aree e i moduli](#cap1-moduli)
   - [I ruoli](#cap1-ruoli)
   - [Il menu](#cap1-menu)
   - [Le parole che usiamo](#cap1-glossario)
   - [Il tuo lavoro in YW: guida rapida per ruolo](#cap1-rapida)
2. [Primo accesso e configurazione guidata](#cap2)
3. [Sicurezza lavoratore](#cap3)
   - [Come funziona la catena di un allarme](#cap3-catena)
   - [Tipi di allarme](#cap3-tipi)
   - [Dispositivi e app](#cap3-dispositivi)
   - [Badge GPS: collegamento, messa in servizio, comandi](#cap3-badge)
   - [Silenzio del dispositivo (perdita di contatto)](#cap3-silenzio)
   - [Immobilità prolungata](#cap3-immobilita)
   - [Check-in](#cap3-checkin)
   - [Aree (geofence) e piantine indoor](#cap3-geofence)
   - [Regole di sicurezza: trigger e profili di protezione](#cap3-regole)
   - [Escalation e reperibilità](#cap3-escalation)
   - [Rete di ultima istanza](#cap3-ultima)
   - [Centrale allarmi](#cap3-centrale)
   - [Cosa succede quando scatta un allarme](#cap3-scatta)
   - [Consegna reale, di prova o simulata](#cap3-consegna)
   - [Mappa e report](#cap3-report)
4. [L'app del lavoratore sul telefono](#cap4)
5. [IoT](#cap5)
6. [Lavoro sul campo](#cap6)
   - [Interventi](#cap6-interventi)
   - [Clienti, richieste e contratti](#cap6-clienti)
   - [Listino](#cap6-listino)
   - [Cantieri e presenze](#cap6-cantieri)
   - [Ronde e cruscotto vigilanza](#cap6-ronde)
   - [Pianificazione: calendario, agenda tecnici, turni](#cap6-pianificazione)
   - [Consuntivi](#cap6-consuntivi)
   - [Ore lavorate](#cap6-ore)
7. [Persone e permessi, sedi e asset, privacy e audit](#cap7)
8. [Assistente e agente AI](#cap8)
9. [Superadmin](#cap9)
10. [Domande frequenti e problemi comuni](#cap10)
- [Appendice tecnica](#appendice)
- [Per saperne di più](#per-saperne-di-piu)

---

<a id="cap1"></a>
## 1. Cos'è YouWorkforce e come è organizzato

YouWorkforce protegge chi lavora da solo o in ambienti a rischio e organizza il lavoro sul campo.
Riceve i segnali da badge GPS, telefoni e sensori, li trasforma in allarmi e avvisa le persone
giuste finché qualcuno non prende in carico l'emergenza. Gestisce anche interventi, cantieri,
ronde, turni, ore e consuntivi.

Ogni azienda cliente (in YW si chiama **tenant**) ha i propri utenti, dati e moduli. I dati di
un'azienda non sono mai visibili a un'altra.

YouWorkforce funziona **in autonomia**: escalation, reperibilità, ricezione dei dati dai badge e
piattaforma IoT sono parte del prodotto, senza servizi esterni. Fanno eccezione solo:
- il **fornitore AI**, se l'azienda lo vuole usare;
- i **canali che avvisano i reperibili** fuori dalla console: SMS e chiamate, Slack, email.
  Li configura il superadmin.

Si accede da tre punti:

| Dove | Per chi | Indirizzo tipico |
|---|---|---|
| **Console** web | amministratori, operatori, reperibili, responsabili, pianificatori, auditor (e lavoratori per alcune pagine) | il dominio dell'azienda, ad esempio `acme.<dominio>` |
| **App del lavoratore** sul telefono | lavoratori, tecnici, guardie | lo stesso dominio, percorso `/worker/` |
| **Console superadmin** | il gestore della piattaforma | pagina superadmin |

La console si usa anche da telefono e tablet: sotto una certa larghezza il menu diventa un
pannello a scomparsa (pulsante con le tre righe in alto a sinistra). In alto a destra si cambiano
**tema** chiaro/scuro e **lingua** italiano/inglese.

<a id="cap1-moduli"></a>
### Le due grandi aree e i moduli

Le funzioni si attivano per azienda con i **moduli**, che accende o spegne il superadmin. Una voce
di menu compare solo se il suo modulo è attivo. Le due aree principali funzionano da sole o
insieme.

| Area | Modulo | Cosa offre |
|---|---|---|
| Sicurezza lavoratore | **Lone Worker e centrale allarmi** | Centrale allarmi, SOS e check-in dall'app, profili di protezione, escalation |
| | **Localizzazione GPS e badge** | badge e tracker GPS (per esempio Teltonika GH-5200), telefoni come tracker; SOS, caduta, immobilità, silenzio del dispositivo |
| IoT | **Dispositivi e IoT industriale** | sensori, telemetria, regole e soglie, monitoraggio, alert IoT, cruscotti degli scenari |
| Lavoro sul campo | **Interventi** | ordini di lavoro, clienti, listino, richieste, contratti, agenda tecnici |
| | **Cantieri** | commesse di cantiere, avanzamento, presenze, costi, SAL e fatture |
| | **Ronde e punzonature** | percorsi di ronda con punti QR, NFC, beacon o GPS; ronda mancata |
| | **Forza lavoro e turni** (gestione turni) | turni pianificati e svolti, reperibilità, assenze, copertura minima |
| | **Ore, spese e consuntivi** | consuntivi con approvazione |
| | **Qualifiche e scadenze** | qualifiche delle persone, usate da turni e listino |
| Trasversale | **Assistente YouWorker.ai e diario vocale** | assistente e agente AI, diario vocale |

Sempre presenti, senza modulo: persone, gruppi di persone, sedi, asset, permessi e ruoli,
privacy e audit, dashboard.

Alcune funzioni di sicurezza servono a più moduli. Basta che sia attivo **uno** dei moduli
indicati:

| Funzione | Moduli che la attivano |
|---|---|
| SOS dall'app del lavoratore | Lone Worker, Localizzazione GPS e badge, IoT, Ronde (serve una Centrale che lo veda) |
| Check-in, silenzio dell'app, battito dell'app | Lone Worker |
| Turno nell'app del lavoratore | Forza lavoro e turni |
| Silenzio di badge e tracker | Localizzazione GPS e badge |
| Immobilità | Lone Worker oppure Localizzazione GPS e badge |
| Ronda mancata | Ronde |
| Squadre | Forza lavoro e turni oppure Cantieri |

Il modulo **Ronde** porta con sé Centrale allarmi ed escalation anche senza la Sicurezza
lavoratore, perché una ronda non svolta apre l'allarme «Ronda mancata».

<a id="cap1-ruoli"></a>
### I ruoli

Ogni persona ha un **ruolo base**. L'amministratore può aggiungere permessi con i **ruoli
personalizzati** (vedi [capitolo 7](#cap7)).

| Ruolo | Cosa fa di solito |
|---|---|
| **Amministratore** | configura tutto: persone, sedi, dispositivi, regole, escalation, moduli e AI, privacy |
| **Operatore centrale** | segue la Centrale allarmi, prende in carico e chiude gli allarmi, invia comandi ai dispositivi |
| **Reperibile** | riceve gli avvisi e gestisce gli allarmi che gli arrivano |
| **Responsabile** | supervisiona allarmi, approva consuntivi, vede ordini e costi |
| **Pianificatore** | crea e assegna interventi, ronde, turni, cantieri |
| **Lavoratore** | usa l'app sul telefono: SOS, check-in, turno, interventi, ronde, diario |
| **Auditor (sola lettura)** | consulta allarmi, consuntivi, ordini, costi e configurazione senza modificarli |

Chi ha **sedi assegnate** vede e gestisce solo quelle (allarmi, ordini, consuntivi), tranne
l'amministratore. Gli allarmi senza sede sono visibili a tutti gli operatori.

<a id="cap1-menu"></a>
### Il menu

Il menu è diviso in cinque gruppi. Alcune voci hanno **schede** (una barra sotto il titolo della
pagina). Vedi solo le voci e le schede che il tuo ruolo e i moduli attivi permettono.

| Gruppo | Voci (tra parentesi le schede) |
|---|---|
| **Operatività** | Dashboard · Centrale allarmi (Tutti · Assegnati a me) · Mappa (Allarmi · Dispositivi · Cantieri) · Piantine indoor · Agente AI |
| **Sicurezza Lavoratore** | Dispositivi e app · Regole di sicurezza (Trigger · Profili di protezione) · Escalation e reperibilità · Report |
| **IoT Dashboard** | Monitoraggio IoT · Alert IoT · Regole sensori · i cruscotti degli scenari attivi |
| **Lavoro sul campo** | Interventi · Clienti · Listino · Richieste · Contratti · Cantieri · Ronde · Cruscotto vigilanza · Consuntivi · Ore lavorate · Pianificazione (Calendario · Agenda tecnici · Turni) |
| **Configurazione** | Configurazione guidata · Collegamento dispositivi · Persone e permessi · Sedi e asset · Privacy e audit · Moduli e AI (Moduli · AI) |

I vecchi link salvati (per esempio a «I miei allarmi», «Mappa dispositivi», «Calendario», «Trigger
di sicurezza») portano da soli alla voce e alla scheda nuove.

**Cose utili in tutta la console**
- Ogni menu a tendina ha una **ricerca**: scrivi una o più parole per filtrare le voci; frecce e
  Invio per scegliere, Esc per chiudere.
- I campi di caricamento file accettano anche il **trascinamento** del file.
- Le tabelle vuote spiegano cosa serve e propongono il pulsante giusto.
- Se una pagina non si carica compare «Impossibile caricare i dati» con **Riprova**. Gli errori
  sono spiegati in italiano (connessione assente, sessione scaduta, permesso mancante…).
- Con la lingua inglese il menu e le date sono tradotti; il resto delle pagine resta in italiano,
  con la traduzione automatica delle frasi più comuni.
- La console si usa anche da tastiera: «Vai al contenuto», Tab per spostarsi, Invio o Spazio per
  attivare, Esc per chiudere pannelli e finestre.
- Dopo un aggiornamento del sistema, se qualcosa sembra strano, ricarica la pagina con
  `Ctrl+Shift+R`.

<a id="cap1-glossario"></a>
### Le parole che usiamo

Ogni cosa ha un solo nome in tutta YW (elenco completo in [glossario](glossario.md)):

- **Gruppo di persone**: un insieme di persone dell'anagrafica (squadra, reparto, zona). Si usa in
  tutti i moduli.
- **Gruppo di escalation**: chi riceve gli allarmi, con la rotazione dei reperibili e la sequenza
  dei passi. Può prendere i membri da uno o più gruppi di persone.
- **Turno pianificato**: il turno di lavoro messo in calendario.
- **Turno in servizio**: il lavoratore ha aperto il turno nell'app; in questo periodo il silenzio
  del telefono e del badge è sorvegliato.
- **Turno di reperibilità**: il periodo in cui una persona è il reperibile di turno del suo gruppo
  di escalation.
- **Percorso di ronda**: la sequenza di punti di controllo da punzonare; una **ronda** è un
  passaggio di una guardia sul percorso.
- **Evento** e **allarme**: un evento viene registrato; un allarme apre un incidente in Centrale e
  avvia l'escalation. Gli allarmi **salva-vita** (SOS, uomo a terra, immobilità, check-in mancato,
  perdita contatto) non si perdono mai.

<a id="cap1-rapida"></a>
### Il tuo lavoro in YW: guida rapida per ruolo

**Amministratore dell'azienda**
1. Apri **Configurazione guidata** e completa i passi indicati ([capitolo 2](#cap2)).
2. Crea sedi, invita le persone, configura reperibilità e recapiti.
3. Collega i dispositivi da **Dispositivi e app** e mettili in servizio.
4. Imposta la **rete di ultima istanza** in Escalation e reperibilità.
5. Controlla in Centrale che non ci sia l'avviso «Gli allarmi non arrivano ai reperibili».

**Operatore centrale**
1. Tieni aperta **Centrale allarmi** (scheda *Tutti*). Un nuovo allarme fa partire un suono e un
   riquadro rosso da qualunque pagina.
2. Apri l'allarme, guarda **dove si trova** il lavoratore, **prendilo in carico**.
3. Gestisci l'emergenza, aggiungi note, **chiudi** con l'esito.

**Reperibile**
1. Ricevi SMS, chiamate o email. Rispondi `OK <codice>` per prendere in carico o `CHIUDI <codice>`
   per chiudere, oppure premi **1** durante la chiamata.
2. In console la Centrale si apre sulla scheda **Assegnati a me**.
3. Verifica il tuo telefono in Escalation e reperibilità (lo fa l'amministratore con te).

**Responsabile**
1. In Centrale, scheda **Assegnati a me**: gli allarmi presi in carico da te o notificati a te.
2. In **Consuntivi** approva o rimanda i consuntivi delle persone (non i tuoi).
3. In **Ore lavorate** confronta turni pianificati e svolti.

**Pianificatore**
1. Crea gli **Interventi** e assegnali in **Pianificazione → Agenda tecnici**.
2. Prepara i **percorsi di ronda** e i loro orari in **Ronde**.
3. Pianifica i **turni** in **Pianificazione → Turni**, segui i **Cantieri**.

**Lavoratore**
1. Apri l'app del lavoratore sul telefono (`/worker/`) e accedi.
2. Inizia il turno, se l'azienda usa i turni. Tieni l'app aperta e il telefono acceso.
3. In pericolo premi **SOS**. Per il resto: interventi del giorno, ronde, note di lavoro
   ([capitolo 4](#cap4)).

**Auditor**
- Consulti Centrale, report, consuntivi, ordini, costi e configurazione in sola lettura. Se ti è
  stato dato il permesso, anche il registro attività in **Privacy e audit**.

---

<a id="cap2"></a>
## 2. Primo accesso e configurazione guidata

### Accedere
- **Console**: email e password sul dominio della tua azienda (es. `acme.…`): l'azienda è quella
  dell'indirizzo. Per entrare in un'altra azienda scrivi il suo nome nel campo **Tenant** e premi
  **Accedi**: si apre l'indirizzo di quell'azienda. In un ambiente locale senza sottodominio
  (`localhost`) l'azienda si scrive nel campo e resta ricordata.
- Il **primo amministratore** riceve dal superadmin un **link di attivazione** (valido 7 giorni,
  usabile una volta): lo apre, sceglie la propria password (almeno 10 caratteri) e accede. Nessuno
  conosce la sua password.
- **Tentativi sbagliati**: dopo 5 errori in 5 minuti dallo stesso indirizzo, quell'utenza è
  bloccata da quell'indirizzo per 5 minuti. Dopo 20 errori in 5 minuti sulla stessa utenza, da
  qualunque indirizzo, è bloccata per 5 minuti anche con la password giusta. Aspetta e riprova.
- Le pagine di accesso mostrano password precompilate e il bollino DEMO solo negli ambienti di
  prova, mai in produzione.

### Configurazione guidata
Menu **Configurazione → Configurazione guidata**. Mostra i passi da completare, con la percentuale
fatta. I passi si spuntano da soli man mano che configuri. **Configurazione guidata** apre il
prossimo passo mancante con la spiegazione e il pulsante che porta alla pagina giusta; **Salta
per ora** passa al successivo.

I passi dipendono dai moduli attivi:

| Passo | Perché serve | Dove si fa |
|---|---|---|
| Crea le sedi | i luoghi di lavoro a cui colleghi dispositivi, turni e allarmi | Sedi e asset |
| Invita utenti e assegna i ruoli | le persone accedono secondo il ruolo | Persone e permessi |
| Configura la reperibilità | senza un gruppo di default, gli allarmi non raggiungono nessuno | Escalation e reperibilità |
| Verifica i recapiti | SMS e chiamate partono solo verso telefoni verificati | Escalation e reperibilità → Recapiti |
| Instrada gli allarmi | a quale gruppo va ogni tipo di allarme | Escalation e reperibilità → Routing |
| Imposta le regole dei sensori | le soglie decidono quando un dato diventa un allarme | Regole sensori |
| Collega i dispositivi | badge, tracker o sensori, con procedura guidata | Dispositivi e app |
| Aggiungi i clienti | i committenti di interventi e commesse | Clienti |

### La dashboard
La **Dashboard** mostra riquadri (widget) secondo i tuoi permessi e i moduli attivi: allarmi aperti,
dispositivi IoT (in linea, che non comunicano, allarmi aperti), interventi, ronde, turni,
consuntivi e altro. Un riquadro compare se hai il permesso per quello che mostra: per esempio,
dando a un ruolo il permesso di vedere la Centrale compare anche il riquadro degli allarmi. Il riquadro **Cosa fare adesso** elenca le azioni in
sospeso per te (allarmi da prendere in carico, interventi da gestire, turni da confermare, i tuoi
interventi). Chi ha il permesso «Dashboard · Organizzare» può scegliere, ordinare e
dimensionare i riquadri per tutta l'azienda con **Organizza**.

---

<a id="cap3"></a>
## 3. Sicurezza lavoratore

<a id="cap3-catena"></a>
### Come funziona la catena di un allarme

1. Un **dispositivo** (badge, telefono, sensore) segnala qualcosa: pulsante SOS, caduta, silenzio,
   check-in scaduto, ingresso in una zona vietata.
2. YW crea un **allarme** (in console anche «incidente») e lo mostra in **Centrale allarmi**, con
   il lavoratore coinvolto e dove si trova.
3. Parte l'**escalation**: YW avvisa i reperibili del gruppo giusto (SMS, chiamata, email, Slack)
   e insiste, passo dopo passo, finché qualcuno non **prende in carico**.
4. Se nessuno risponde o nessuno è disponibile, per gli allarmi critici interviene la **rete di
   ultima istanza**.
5. Chi gestisce l'emergenza annota e **chiude** l'allarme con l'esito. Tutto resta nella
   cronologia dell'allarme.

Stati di un allarme: **Aperto** → **Preso in carico** → **Chiuso** (in mezzo può comparire «In
escalation»).

Se lo stesso badge ripete l'SOS o la caduta mentre l'allarme è ancora aperto, non nasce un secondo
allarme: la segnalazione ripetuta si aggiunge alla cronologia di quello esistente e l'escalation
prosegue senza raffiche di messaggi.

<a id="cap3-tipi"></a>
### Tipi di allarme

| Allarme | Da dove arriva |
|---|---|
| **SOS / richiesta soccorso** | pulsante del badge o dell'app |
| **Uomo a terra** | sensore di caduta del badge |
| **Immobilità prolungata** | il lavoratore resta fermo oltre la soglia |
| **Check-in mancato** | il lavoratore non conferma il check-in in tempo |
| **Perdita di contatto** | badge o app in servizio che smettono di trasmettere |
| **Ingresso in zona vietata / uscita da area sicura** | aree disegnate sulla mappa o sulla piantina |
| **Batteria bassa / in esaurimento** | livello batteria sotto soglia (secondo l'impostazione dell'azienda) |
| **Manomissione, perdita alimentazione, disturbo del segnale** | segnali del dispositivo |
| **Sensore critico / allarme sensore** | regole dei sensori IoT |
| **Ronda mancata** | ronda pianificata non avviata in tempo |

Sono **salva-vita** SOS, uomo a terra, immobilità, check-in mancato e perdita di contatto. Questi
allarmi non vengono mai sospesi dalla manutenzione e non si chiudono «in blocco» senza una
conferma esplicita.

Per gli altri eventi dei badge (eccesso di velocità, movimento non accompagnato, ingresso o
uscita da un'area…) l'azienda decide cosa fare: **ignora**, **registra** oppure **allarme con
escalation**. Si imposta in **Collegamento dispositivi → Come trattare gli eventi**.

<a id="cap3-dispositivi"></a>
### Dispositivi e app

Menu **Sicurezza Lavoratore → Dispositivi e app** (amministratore o chi ha il permesso
«Amministrazione · Dispositivi»).

La tabella elenca tutti i dispositivi con identificativo, modello, etichetta, a chi è assegnato,
capacità, ultimo contatto e stato. Il campo di ricerca filtra su tutte le colonne (più parole
insieme) e mostra quanti ne trova.

**Come aggiungere un badge o un tracker**
1. **+ Aggiungi (guidato)**.
2. Scegli il modello dal catalogo (filtri per produttore e uso: lavoratore, smartphone, veicolo,
   attrezzatura). Il catalogo completo dei tracker generici compare solo se scrivi una ricerca.
3. Inserisci l'IMEI. Con **Scansiona** puoi leggere il QR o il codice a barre dall'etichetta con la
   fotocamera (serve un browser normale in https, non una finestra incorporata).
4. Scegli lavoratore e, se serve, gruppo di risposta. La procedura crea anche il profilo di
   protezione del lavoratore e l'instradamento dei suoi allarmi.
5. Segui la **guida di collegamento** che compare alla fine: per i Teltonika server e porta da
   impostare sul dispositivo; per un telefono l'indirizzo da incollare nell'app di tracciamento.

Altri modi: **Import massivo** (molti dispositivi da un file CSV), **manuale**, e per i sensori
**Attiva per un lavoro (IoT)** (vedi [capitolo 5](#cap5)).

Il gruppo scelto nell'onboarding vale solo per quel lavoratore: non cambia l'instradamento degli
allarmi del resto dell'azienda.

**Scheda del dispositivo** (clic sulla riga):
- **Ultima posizione** con batteria e **Mostra indirizzo**. L'indirizzo lo chiede il tuo browser a
  OpenStreetMap solo quando premi il pulsante; il server non invia le posizioni all'esterno.
- **Stato / readiness**: cosa manca perché il dispositivo protegga davvero il lavoratore, e
  **SOS di prova**, che manda un SOS lungo il percorso reale: controlla che compaia in Centrale.
- Telemetria, mappa dei dati, attributi e comandi (per i sensori).
- Per i badge GPS: **Collegamento**, **stato di servizio**, comandi rapidi, percorsi e soste (vedi
  sotto).

<a id="cap3-badge"></a>
### Badge GPS: collegamento, messa in servizio, comandi

**Collegare un Teltonika.** Sul dispositivo (con il configuratore Teltonika o via SMS) imposta come
server l'indirizzo di YouWorkforce e la porta TCP **5027**. Il badge si riconosce dall'IMEI già
registrato: non c'è nessuna chiave da copiare. Telefoni e altri tracker usano una **chiave di
collegamento** (vedi [Collegamento dispositivi](#collegamento-dispositivi)).

**Il badge non si collega?** Nella scheda, riquadro **Collegamento**, premi **verifica ora**:
- *Collegato adesso* — tutto bene;
- *Non collegato* (con l'ultimo contatto) o *Mai collegato* — controlla server, porta, SIM;
- *Rifiutato*, con il motivo: IMEI non registrato (registralo con l'IMEI esatto), IMEI registrato
  in più aziende (va eliminato il doppione sbagliato, chiedi al superadmin), modulo dispositivi
  spento (va attivato).

Lo stesso IMEI non si può registrare in due aziende diverse.

**Mettere in servizio.** Assegnare un badge a un lavoratore non basta: va **messo in servizio**
con il pulsante **Metti in servizio** nella scheda. Solo da quel momento YW sorveglia il suo
silenzio.

| Stato di servizio | Significato | Allarme di silenzio |
|---|---|---|
| **In servizio** | il badge protegge il lavoratore assegnato | sì |
| **In attesa** | registrato o assegnato, non ancora in servizio | no |
| **Fuori servizio** | ritirato, guasto, in magazzino, disabilitato | no |

- Se passi il badge a un altro lavoratore, il conteggio del silenzio riparte.
- Senza lavoratore il badge torna *in attesa*; disabilitato passa *fuori servizio*. Rimetterlo in
  servizio è sempre un'azione esplicita.
- Un badge messo in servizio che non si è mai collegato fa scattare l'allarme alla soglia.

**Comandi rapidi** (con il badge collegato): chiedi la posizione adesso, stato, informazioni di
rete e SIM, versione firmware, riavvia. L'esito finisce nello storico comandi. Il **comando
libero** per i tecnici può riconfigurare il dispositivo: è riservato a chi amministra i
dispositivi.

**Percorsi e soste**: nella scheda, gli ultimi 7 giorni con export CSV. Una sosta è una fermata di
almeno 5 minuti entro 50 metri; un tragitto è il movimento tra due soste se supera 100 metri. Su
periodi con moltissime posizioni il risultato viene dichiarato **parziale** con un avviso rosso.

**Batteria**: sotto la soglia impostata (in **Collegamento dispositivi → Soglie e allarmi**) YW
segnala una volta per ogni discesa: come semplice evento o come allarme, secondo la scelta
dell'azienda.

<a id="collegamento-dispositivi"></a>
**Collegamento dispositivi** (menu Configurazione). Riassume quanti badge sono in servizio,
quanti sono assegnati ma non in servizio e dopo quanto il silenzio diventa allarme. Da qui:
- **Soglie e allarmi**: soglia di silenzio, «solo durante il turno», batteria, filtri sulle
  posizioni implausibili;
- **Come trattare gli eventi**: ignora, registra o allarme per gli eventi non salva-vita;
- **Chiavi di collegamento**: crea una chiave per telefoni, tracker HTTP o sensori, legata a un
  dispositivo (consigliato) o a tutta l'azienda, con scadenza facoltativa. La chiave si vede **una
  sola volta** alla creazione: copiala subito. Si può revocare o eliminare.

<a id="cap3-silenzio"></a>
### Silenzio del dispositivo (perdita di contatto)

Un dispositivo che smette di trasmettere (spento, scarico, senza copertura, manomesso) non protegge
più il lavoratore. YW lo segnala con l'allarme **Perdita di contatto**.

**Badge e tracker**
- Si sorveglia ogni badge **in servizio**. Soglia predefinita: **10 minuti** (si cambia in
  Collegamento dispositivi → Soglie e allarmi; 0 la disattiva).
- **Sempre** (impostazione predefinita) oppure **solo durante il turno** del lavoratore (turno
  aperto nell'app o turno pianificato in corso). Attenzione: con «solo durante il turno» un'azienda
  che non usa i turni non riceve allarmi di silenzio. Fuori turno la scheda del badge dice «fuori
  turno · silenzio non sorvegliato».
- Un badge al chiuso che manda solo segnali di vita, senza posizione, non è considerato
  silenzioso.
- Un silenzio apre un solo allarme. Quando il badge torna in linea l'allarme **si chiude da solo**
  con esito «rientrato» e chi era stato avvisato riceve la chiusura.

**App del lavoratore**
- Serve il modulo **Forza lavoro e turni**: il turno nell'app esiste solo con questo modulo. Con il
  solo Lone Worker l'app non apre turni, quindi il suo silenzio non è sorvegliato; restano
  check-in, SOS e immobilità.
- Durante il turno l'app dà un segno di vita ogni minuto. Se tace oltre **15 minuti** (predefinito,
  in **Moduli e AI → Moduli → Lone Worker → Configura strumenti → Silenzio dell'app**) parte la
  perdita di contatto, che si chiude da sola quando l'app torna.
- **Preavviso via SMS**: 2 minuti prima della soglia (impostabile, 0 = nessun preavviso) il
  lavoratore riceve un SMS che lo invita ad aprire l'app. Se la apre, l'allarme non parte. Un SMS
  per episodio; serve il numero di telefono nel profilo della persona.
- Chi ha un badge in servizio è sorvegliato dal badge: l'app non genera un secondo allarme.
- Limite da conoscere: a schermo bloccato il telefono può sospendere l'app web. Per chi lavora da
  solo in aree a rischio resta consigliato il badge.

<a id="cap3-immobilita"></a>
### Immobilità prolungata

Si attiva nel **profilo di protezione** (Regole di sicurezza → Profili di protezione, sezione
«Rileva l'immobilità»).

| Impostazione | Predefinito | Significato |
|---|---|---|
| Rileva l'immobilità | spento | attiva il controllo per i lavoratori del profilo |
| Fermo per almeno | 5 minuti (1–120) | tempo senza movimento prima dell'allarme |
| Preavviso sull'app | 60 secondi (0–600) | quanto prima dell'allarme compare «Stai bene?» |
| Spostamento minimo | 15 metri (2–500) | uno spostamento più piccolo non conta |
| Solo durante il turno | sì | serve il modulo Forza lavoro e turni; senza, togli la spunta o l'immobilità non scatterà mai |

Come funziona: se il lavoratore resta fermo, l'app mostra a tutto schermo **«Stai bene?»** con
suono e vibrazione; **Sto bene** fa ripartire il conteggio. Alla soglia si apre l'allarme
*Immobilità prolungata*. Sul badge non c'è preavviso. Vale per badge in servizio o app che stanno
trasmettendo; un dispositivo muto è gestito dalla perdita di contatto.

<a id="cap3-checkin"></a>
### Check-in

Il check-in è un «conto alla rovescia di sicurezza»: il lavoratore lo avvia dall'app prima di
un'attività a rischio e deve confermare la presenza prima della scadenza. Se non conferma, si apre
l'allarme **Check-in mancato** con il lavoratore e la sede.

- Durata predefinita e massima si impostano in **Regole di sicurezza → Trigger** (predefinito 30
  minuti, massimo 120). Un profilo di protezione può avere una durata propria.
- Il check-in attivo sopravvive alla chiusura o al ricaricamento dell'app.
- In Centrale si vedono i check-in in corso e quelli scaduti.
- Serve il modulo Lone Worker.

<a id="cap3-geofence"></a>
### Aree (geofence) e piantine indoor

**Aree sulla mappa.** In **Mappa → Dispositivi** l'amministratore disegna le aree, le rinomina,
cambia colore e forma, le associa a dispositivi ed eventi. Mentre disegni scegli il **tipo**:
- *nessun allarme* — solo presenza e registro di chi entra ed esce;
- *zona vietata* — allarme all'**ingresso**;
- *area sicura* — allarme all'**uscita**.

L'allarme porta il dispositivo e il lavoratore reali e il nome dell'area. Se l'area ha dispositivi
associati, scatta solo per quelli. Attraversamenti ripetuti sul confine si aggiungono all'allarme
ancora aperto. Cliccando il nome di un'area nell'elenco la mappa ci zooma sopra.

In Mappa → Allarmi e in Piantine la gestione delle aree è nascosta di default: l'amministratore
può mostrarla con **Mostra aree geofence**.

**Piantine indoor** (menu Operatività). Con **Configura piantina indoor**:
1. carica la piantina (immagine o PDF) e collegala a una sede;
2. registra i **beacon** con il loro identificativo reale, la zona (es. «Reparto verniciatura»),
   sede e piano;
3. sulla piantina, con **Posiziona sulla piantina**, scegli un beacon o un dispositivo e mettilo
   al suo posto.

Il beacon è un punto fisso che definisce una zona: gli allarmi indoor si agganciano alla zona. La
piantina elenca a destra gli allarmi indoor; un clic su un segnaposto porta al messaggio
corrispondente e viceversa.

<a id="cap3-regole"></a>
### Regole di sicurezza: trigger e profili di protezione

Menu **Sicurezza Lavoratore → Regole di sicurezza** (amministratore o permesso «Escalation e
reperibilità»).

**Scheda Trigger.** Elenca gli eventi del lavoratore, da dove arrivano e se sono critici (aprono un
allarme) o solo registrati. Si configurano:
- come instradare l'SOS (SOS oppure uomo a terra) e la sua priorità;
- se il «dispositivo fermo» va considerato immobilità;
- la ripetizione della notifica SOS (secondi, 0 = una volta);
- durata predefinita e massima del check-in.

**Scheda Profili di protezione.** Un profilo lega un insieme di **lavoratori** a un **gruppo di
reperibili**, ai **trigger** attivi (SOS, uomo a terra, immobilità, check-in mancato, aree), al
**check-in** e all'**immobilità**. I dispositivi coperti sono quelli assegnati ai lavoratori del
profilo.

I lavoratori si aggiungono uno per uno oppure **collegando un gruppo di persone** (riga «Membri dai
gruppi di persone», **+ collega**): chi entra nel gruppo di persone entra nel profilo, chi ne esce
ne esce, senza altri passaggi. Queste persone hanno l'indicazione «da «nome del gruppo»» e si tolgono
dal gruppo di persone o scollegando il gruppo. Una persona ha un solo profilo: se ne ha già un
altro, resta in quello.

Se il profilo ha un gruppo, gli allarmi di quei lavoratori vanno a quel gruppo, prima di qualunque
altra regola. Spegnere un trigger nel profilo sopprime davvero quegli allarmi per i suoi
lavoratori. Il gruppo del profilo vale anche quando il lavoratore usa un badge registrato a nome di
un collega.

<a id="cap3-escalation"></a>
### Escalation e reperibilità

Menu **Sicurezza Lavoratore → Escalation e reperibilità** (amministratore o permesso «Escalation e
reperibilità»). La voce c'è con qualunque modulo che produce allarmi: Lone Worker, Localizzazione
GPS e badge, IoT o Ronde. Le modifiche valgono subito. La pagina ha sei schede.

**A chi va un allarme (ordine di precedenza)**
1. il gruppo del **profilo di protezione** del lavoratore;
2. il gruppo scelto nel **routing** per quel tipo di allarme;
3. il **gruppo di default**;
4. se non c'è nessuno, e l'allarme è critico, la **rete di ultima istanza**.

**Scheda Gruppi**
1. **+ Nuovo gruppo** (es. «Reperibili notte») e aggiungi i membri: uno per uno, oppure collegando
   uno o più **gruppi di persone** (**+ collega**). Con un gruppo collegato il gruppo di escalation
   resta sempre allineato: chi entra nel gruppo di persone va in coda alla rotazione, chi ne esce
   viene tolto. Queste persone hanno il simbolo 👥; quelle aggiunte a mano non vengono mai toccate.
2. Scegli il **gruppo di default**, che riceve tutto ciò che non ha un routing specifico. Senza,
   un allarme rischia di non arrivare a nessuno.
3. Reperibilità del gruppo: **tutti insieme** oppure **a turni** (orario, giornaliero,
   settimanale, mensile) con durata e passaggio di turno. Fuso orario del gruppo per orari e cambi
   d'ora. **Attiva ora** rende subito reperibile una persona.

Dentro ogni gruppo ci sono sotto-schede:
- **Copertura · passaggio · Slack**: 24 ore su 24 oppure solo fasce orarie e giorni; ora del
  passaggio di turno; canale Slack e avviso automatico di chi è di turno.
- **Policy**: la sequenza dei passi di escalation. Per ogni passo canale (SMS, voce, email, Slack),
  destinatario (reperibile del gruppo, un altro gruppo, una persona, un canale Slack) e attesa
  prima del passo dopo. Si possono riordinare e ripetere N volte. Esempio: passo 1 reperibile,
  passo 2 responsabili, passo 3 direzione. Senza passi personalizzati vale «SMS subito, poi
  chiamata dopo i minuti di fallback».
- **Label**: etichette per ritrovare i gruppi.
- **Playbook**: le istruzioni «cosa fare» per il reperibile, e le note di passaggio turno.
- **Assegnazioni**: coperture aggiuntive con propri membri e finestra (es. «Weekend»).
- **Turni**: il calendario calcolato dei prossimi 30 giorni, in lista o a griglia, filtrabile per
  persona; link calendario (iCal) del gruppo, che si può rinnovare o revocare.

**Scheda Routing**: per ogni tipo di allarme scegli il gruppo. L'elenco comprende tutti i tipi che
aprono un allarme.

**Scheda Notifiche**
- **Fallback vocale**: se l'SMS non riceve risposta entro N minuti parte una chiamata
  (consigliato, predefinito 5).
- **Avvisa sugli aggiornamenti**: il reperibile sa se un collega prende in carico o chiude.
- **Manutenzione**: sospende per 1, 2 o 4 ore l'escalation degli allarmi **non** salva-vita
  (sensori, dispositivi…). A fine finestra arriva un riepilogo unico. **Termina** la chiude subito.
  I salva-vita passano sempre.
- **Il mio calendario reperibilità**: link iCal con i tuoi turni per Google o Apple Calendar.

**Scheda Recapiti**
- Per ogni persona il telefono e lo stato di verifica: **invia codice** (arriva un SMS),
  **inserisci codice**, **prova** (SMS di test). Verifica sempre i recapiti: la verifica conferma che il numero è
  giusto, ma gli avvisi partono anche verso un numero non ancora verificato (meglio un avviso in
  più che uno perso), quindi un numero sbagliato si scopre solo con la verifica.
- Recapiti aggiuntivi (SMS, voce, email) e **regole personali**: quale recapito e dopo quanti
  minuti (es. SMS subito, chiamata dopo 2 minuti). Gli avvisi personali continuano fino alla presa
  in carico, anche se nel frattempo l'escalation è passata al passo successivo.
- **Opt-out**: chi risponde STOP a un SMS o preme 9 durante una chiamata non riceve più nulla su
  quel recapito. Per riattivarlo risponde START, oppure l'amministratore preme **Riabilita**; poi
  il recapito va verificato di nuovo. Lo stato è segnato in rosso.

Il telefono principale della persona si inserisce in **Persone e permessi** (formato
internazionale, es. `+393331234567`).

**Scheda Assenze & turni**
- **Assenze e sostituzioni**: per un periodo togli una persona e/o metti un sostituto.
- **Pianificazione temporanea**: per un periodo sostituisci la reperibilità del gruppo con blocchi
  «persona + ore».

**Scheda Monitor**
- Diagnostica delle consegne.
- **Heartbeat monitor**: un sistema esterno chiama periodicamente un indirizzo; se smette oltre il
  tempo impostato (predefinito 15 minuti) si apre un allarme, che si chiude da solo alla ripresa.
- **Email → allarme**: un indirizzo email dell'azienda; un messaggio ricevuto lì diventa un
  allarme instradato.

<a id="cap3-ultima"></a>
### Rete di ultima istanza

In **Escalation e reperibilità** c'è il riquadro **Rete di ultima istanza**: un elenco di contatti
email e/o SMS dell'azienda (fino a 50). Interviene per un allarme **critico** quando:
- il passo di escalation non ha destinatari (fuori copertura, gruppo senza recapiti);
- l'allarme non ha alcun gruppo (né profilo, né routing, né default);
- tutti i passi sono finiti e nessuno ha preso in carico.

Modalità **tutti insieme** oppure **uno dopo l'altro** (intervallo da 30 secondi a un'ora,
predefinito 2 minuti). La prima presa in carico ferma la catena. **Configurarla è necessario**: se
è vuota la pagina avvisa che questi allarmi non arrivano a nessuno.

<a id="cap3-centrale"></a>
### Centrale allarmi

Menu **Operatività → Centrale allarmi**.
- Schede **Tutti** e **Assegnati a me**. Operatore e amministratore partono da *Tutti*;
  reperibile e responsabile da *Assegnati a me* (allarmi presi in carico da te o per cui sei stato
  avvisato).
- Filtri **Aperti · Chiusi · Tutti** con il conteggio; predefinito *Aperti*, ordinati per
  priorità. Gli aperti ci sono tutti; dei chiusi si vedono gli ultimi 300.
- **Ricerca** su tipo, sede, assegnato, stato, priorità, zona, piantina, beacon, riferimento.
- 25 allarmi per pagina. La pagina ricorda scheda e ricerca.
- In alto: chi è **di turno** in questo momento, i check-in in corso e scaduti.
- Ogni riga mostra il lavoratore e la posizione: *Interno* (piantina e zona) oppure coordinate o
  sede. Gli allarmi aperti e non ancora presi in carico **lampeggiano** in rosso.

**Allarme in arrivo.** Da qualunque pagina della console: suono a tre impulsi, finestra rossa
«NUOVO ALLARME» (accumula più allarmi), riga lampeggiante. Il browser attiva l'audio dopo il primo
clic sulla pagina: all'inizio del turno fai un clic qualsiasi.

**Gestire un allarme** (clic sulla riga):
- **Dove si trova**: *Apri piantina* (interno) o *Apri in mappa* (esterno);
- **Prendi in carico** — ferma l'escalation;
- **Assegna** a un collega, **aggiungi una nota**;
- **Chiudi** con l'esito (es. «risolto sul posto»);
- cronologia completa: avvisi inviati, risposte, note, rete di ultima istanza;
- sintesi AI dell'allarme, se attiva ([capitolo 8](#cap8)).

Ogni allarme ha un riferimento breve (es. `#AB12CD34`), lo stesso che arriva nei messaggi.

**Operazioni in blocco** (operatore, amministratore): **Prendi in carico tutti** e **Chiudi
tutti**. «Chiudi tutti» lascia aperti i salva-vita; per chiudere anche quelli serve una seconda
conferma: «ho verificato che ogni persona è al sicuro».

<a id="cap3-scatta"></a>
### Cosa succede quando scatta un allarme

Esempio: Mario, solo in un capannone, cade e il badge segnala «uomo a terra».

1. L'allarme compare subito in Centrale con suono e finestra rossa: «Uomo a terra – Mario –
   Capannone A, zona Verniciatura».
2. Il reperibile del gruppo riceve un SMS del tipo: «ALLARME Uomo a terra - mario@… - Capannone A -
   posizione https://… Rispondi OK 1a2b3c4d per prenderlo in carico, CHIUDI 1a2b3c4d per
   chiuderlo.»
3. Se non risponde entro i minuti del fallback, riceve una chiamata: **1** prende in carico.
4. Se ancora nessuno risponde, la policy passa al gradino successivo (es. i responsabili). Alla
   fine, la rete di ultima istanza.
5. Il primo che prende in carico (da SMS, chiamata o console) ferma la catena. Gli altri, se lo
   hanno scelto, ricevono l'aggiornamento.
6. Chi interviene chiude l'allarme con l'esito.

**Rispondere a un SMS**: la parola deve essere la **prima** del messaggio. `OK`, `ACK`, `1` o
`PRESO` = presa in carico; `CHIUDI` o `RISOLTO` = chiusura. Il codice dopo la parola indica
l'allarme. Senza codice la risposta vale solo se hai un unico allarme da gestire; altrimenti ricevi
l'elenco dei codici. Valgono solo gli allarmi per cui quel numero è stato avvisato.

Con molti allarmi aperti YW serve per primi quelli a priorità più alta: un SOS non resta in coda.

<a id="cap3-consegna"></a>
### Consegna reale, di prova o simulata

Gli avvisi fuori dalla console (SMS, chiamate, email, Slack) dipendono dai **canali di consegna**
configurati dal superadmin. La consegna può essere:
- **reale** — SMS e chiamate arrivano davvero ai telefoni;
- **di prova** — gli avvisi vanno a un ricevitore di test, non ai telefoni;
- **simulata** — nessun canale configurato: nessun reperibile viene avvisato.

Se la consegna non è reale, chi segue gli allarmi vede in Centrale e in Escalation l'avviso **«Gli
allarmi non arrivano ai reperibili»** (o «di prova»). In quel caso avvisa subito il superadmin. La
Centrale in console continua comunque a mostrare gli allarmi.

<a id="cap3-report"></a>
### Mappa e report

**Mappa** (Operatività), con le schede:
- **Allarmi**: allarmi con posizione, interni ed esterni, filtrabili per periodo e tipo. Un clic
  su una riga dell'elenco centra la mappa sul segnaposto con un anello pulsante e un riepilogo;
  gli allarmi senza coordinate aprono direttamente l'allarme.
- **Dispositivi**: posizioni in tempo reale, aree, presenza nelle aree.
- **Cantieri**: i cantieri sulla mappa (modulo Cantieri).

**Report** (Sicurezza Lavoratore): **Allarmi** (totale, critici, aperti, presi in carico, tempo
medio di risposta) e **Movimenti** (percorsi, soste, presenza nelle aree), con export CSV, TXT e
PDF.

---

<a id="cap4"></a>
## 4. L'app del lavoratore sul telefono

Si apre dal browser del telefono all'indirizzo dell'azienda, percorso `/worker/`. Si può
installare sulla schermata principale come un'app. Si accede con azienda (se non è già nel
dominio), email e password.

L'app mostra solo le funzioni dei moduli attivi.

| Sezione | Quando compare |
|---|---|
| SOS, condivisione posizione, check-in | modulo Lone Worker |
| Turno e turni pianificati | modulo Forza lavoro e turni |
| Oggi · ordini assegnati | modulo Interventi |
| Ronde | modulo Ronde e punzonature |
| Note di lavoro (diario) | modulo Forza lavoro e turni o Interventi |
| Assistente AI sul campo, dettatura AI | se l'amministratore ha attivato quelle funzioni AI |

### SOS
Premi il grande pulsante rosso **SOS**. La centrale viene allertata con la tua posizione.
- L'SOS viene salvato sul telefono prima ancora di leggere la posizione e riprovato da solo
  (anche se chiudi l'app, ricarichi la pagina o la rete torna dopo) finché la centrale non lo
  riceve. I tentativi ripetuti non creano doppioni.
- Finché non è consegnato vedi un avviso rosso «SOS non ancora consegnato» con il pulsante
  **Chiama 112**. Se sei in pericolo, chiama subito i soccorsi.
- Se in azienda non c'è nessun modulo che possa vedere l'SOS, l'app non lo accetta e ti dice di
  chiamare i soccorsi.

### Condivisione posizione
**Condividi posizione** fa vedere alla centrale dove sei mentre lavori. **Interrompi
condivisione** la ferma. Serve il permesso di posizione del telefono.

### Check-in
1. Nel riquadro **Check-in sicurezza** imposta i minuti e premi **Avvia**.
2. Prima della scadenza premi **Conferma presenza**.
3. Se non confermi, la centrale riceve l'allarme «Check-in mancato».

### Turno
- **Inizia turno** / **Termina turno**: l'app conferma che il server ha registrato l'inizio e la
  fine.
- Durante il turno l'app è **sorvegliata**: tienila aperta e il telefono acceso. Se tace troppo
  a lungo ricevi prima un SMS di preavviso, poi parte l'allarme di perdita di contatto.
- **Turni pianificati**: i turni futuri; **Conferma turno** per quelli da confermare.

### «Stai bene?»
Se sei fermo da troppo tempo (quando l'immobilità è attiva) compare a tutto schermo «Stai bene?»
con suono e vibrazione. Tocca **Sto bene** per far ripartire il conteggio. Se non rispondi parte
l'allarme.

### Interventi del giorno
Nella sezione **Oggi · ordini assegnati**:
1. **Apri intervento**. In evidenza le **indicazioni di accesso** (es. «chiave in portineria»).
2. **Naviga verso il luogo** apre il navigatore del telefono verso le coordinate della sede (o
   l'indirizzo).
3. **Registra arrivo**, poi **Avvia lavoro**.
4. Spunta la **checklist**, scrivi note e report, i minuti lavorati e i **materiali**.
5. Carica le **foto** (serve la rete: le foto non si salvano offline).
6. Scegli l'**esito**: completato, parziale, cliente assente, accesso impedito, ritorno da
   programmare.
7. **Salva bozza** o **Invia rapporto**. Con il modulo consuntivi c'è anche **Genera consuntivo
   dall'intervento** (solo a lavoro completato).

Se la rete cade, il testo resta in coda sul telefono («in attesa») e si invia da solo al ritorno.

### Ronde
1. Scegli il percorso e premi **Avvia ronda**.
2. Per ogni punto premi **punzona**:
   - **QR**: si apre la fotocamera con il mirino; inquadra il codice;
   - **NFC**: avvicina il telefono al tag (Android con Chrome);
   - **GPS**: basta essere vicini al punto;
   - **beacon**: inserisci il codice.
3. Se lo scanner non funziona puoi **digitare il codice** scritto sul punto (**Annulla / digita**).
   Vale, ma viene segnalato come «codice digitato a mano». Sui percorsi «solo scansione» non è
   possibile.
4. **Chiudi ronda**: completata se tutti i punti sono a posto entro il tempo previsto, altrimenti
   incompleta.

Non vedrai mai il codice atteso di un punto: serve a provare che sei passato davvero.

### Note di lavoro (diario)
**Detta una nota di lavoro**: scrivi o detta (riconoscimento vocale del browser; se l'azienda l'ha
attivato anche **Detta (AI)**), collega la nota a un intervento e salva. Le note scritte senza rete
restano «in attesa» e partono al ritorno. Il canale SOS è sempre indipendente da dettatura e
documenti.

### Presenze in cantiere
Le entrate e le uscite di cantiere si timbrano dalla **scheda del cantiere** in console (aperta dal
telefono), con **Timbra entrata** e **Timbra uscita**. Vedi [Cantieri e presenze](#cap6-cantieri).

---

<a id="cap5"></a>
## 5. IoT

Modulo **Dispositivi e IoT industriale**: sensori di temperatura, CO₂, livello, energia, porte,
conteggio persone e altri. Gli alert IoT sono **separati** dalla Centrale allarmi della sicurezza
lavoratore. Se è attivo anche Lone Worker, una soglia critica apre anche un allarme di sicurezza con
escalation.

### Attivare un sensore «per un lavoro»
In **Dispositivi e app → Attiva per un lavoro (IoT)**:
1. scegli il lavoro che deve fare il sensore: catena del freddo, livello serbatoio, energia,
   qualità dell'aria, varchi, conteggio persone, ambiente LoRaWAN; oppure il modello di sensore
   (il catalogo propone l'hardware consigliato per ogni lavoro);
2. inserisci l'identificativo;
3. YW configura tutto: dispositivo, chiave, conversione dei valori, soglie predefinite, controllo
   «offline»; e mostra le **istruzioni di collegamento** per il protocollo (HTTP, MQTT o LoRaWAN).

Nella scheda del sensore: telemetria, **mappa dei dati** (collega i nomi che usa il sensore,
es. `t` o `temp`, alle grandezze standard come *temperatura*, con eventuale scala), attributi e
comandi, collegamento a uno **scenario IoT**.

### Regole sensori
Menu **IoT Dashboard → Regole sensori**. Una regola è: **metrica** + **condizione** + **soglia** →
**evento**. Esempio: *temperatura maggiore di 8* → *Sensore critico*.
- Ogni regola vale per **tutti i dispositivi** oppure solo per un dispositivo, un modello, una sede
  o uno scenario.
- Se scattano più regole, vale la **più grave**. Una regola di tipo «telemetria» registra il dato
  senza aprire alert.
- Clic sulla riga per modificarla. **Suggerisci regole (AI)** propone regole dai dati (se attivo).
- Le regole «uomo a terra» si creano solo a mano.

### Monitoraggio IoT e Alert IoT
- **Monitoraggio IoT**: per ogni sensore stato online/offline, ultimi valori con unità, tendenza
  e alert aperti; riepilogo della flotta. Si aggiorna da solo.
- **Alert IoT**: alert di **soglia**, **offline** (sensore muto oltre il tempo previsto) e
  **ripristino**. Gravità critica, avviso o informazione. Stati aperto → preso in carico → chiuso.
  Se il problema continua mentre l'alert è aperto, ne aumentano le occorrenze senza nuovi avvisi.

### Cruscotti degli scenari
Gli **scenari IoT** (16 ambienti predefiniti, come energia, catena del freddo, SCADA) li attiva il
superadmin. Ogni scenario attivo aggiunge un cruscotto nel gruppo **IoT Dashboard**, con i
dispositivi dimostrativi e i sensori reali collegati allo scenario.

I **dati demo** arrivano come la telemetria di un sensore vero: finiscono nello storico, fanno
scattare le regole sensori e aprono gli **alert IoT**; aprono un allarme in Centrale solo se
l'azienda ha anche la sicurezza lavoratore. Servono il modulo IoT e i dispositivi demo dello
scenario. Comandi come **l'aerazione di un silo** sono comandi veri, registrati tra i comandi del
dispositivo: un dispositivo reale li riceve e risponde con il nuovo stato; un dispositivo demo li
esegue subito.

---

<a id="cap6"></a>
## 6. Lavoro sul campo

Il lavoro sul campo funziona anche senza la sicurezza lavoratore. I moduli sono indipendenti:
un'azienda può avere solo Interventi, solo Ronde, solo Cantieri, o tutti.

<a id="cap6-interventi"></a>
### Interventi

Menu **Lavoro sul campo → Interventi**. Ordini di lavoro per assistenza e manutenzione a visite.

**Creare un intervento** (pianificatore): **+ Nuovo intervento**, poi sede, cliente, titolo,
istruzioni, **checklist** (una voce per riga), persona assegnata oppure **gruppo** (i membri del
gruppo vedono ed eseguono l'ordine), data e ora.

La pagina mostra l'**agenda degli appuntamenti** e gli interventi **da pianificare**. Stati: Bozza,
Assegnato, In corso, Completato, Sospeso, Annullato.

**Chi vede cosa**
- Tutti gli interventi: chi pianifica o ha il permesso «Interventi · Vedere tutti».
- Gli altri: i propri e quelli del proprio gruppo.
- Avviano, completano e vedono gli allegati solo l'assegnatario, i membri del gruppo assegnato e
  il pianificatore.
- Le indicazioni di accesso della sede: solo pianificatore e assegnatario.
- Prezzi dei materiali: solo chi vede i costi.

**Revisione**: nella scheda dell'intervento il responsabile vede arrivo, avvio, minuti,
checklist, materiali, esito, report e allegati (foto, documenti, firma). Un intervento
**completato o annullato** non si modifica più (rapporto, materiali, allegati, pianificazione).

<a id="cap6-clienti"></a>
### Clienti, richieste e contratti

- **Clienti**: anagrafica dei committenti (ragione sociale, dati di fatturazione elettronica,
  termini di pagamento, referenti), con gli interventi collegati.
- **Richieste**: il canale d'ingresso prima dell'intervento (es. «perdita acqua bagno», da chi,
  contatti). Una richiesta si **converte in intervento** (bozza) una sola volta; una richiesta
  chiusa non si converte.
- **Contratti**: contratti con i clienti (SLA di risposta in ore, monte ore, inizio e fine, stato)
  e **piani di manutenzione ricorrente** (ogni N giorni). Un piano **crea da solo l'intervento**
  (in bozza, alle 9:00 del giorno previsto, ora dell'azienda) qualche giorno prima della scadenza
  (**quanti giorni prima**, di solito 7) e sposta avanti la data; togliendo **Crea l'intervento da
  solo** il piano resta manuale. **Genera ora** crea subito l'intervento della prossima scadenza.
  Se un piano è rimasto indietro (scadenze già passate) si crea un solo intervento e la prossima
  data passa alla prima scadenza futura.

<a id="cap6-listino"></a>
### Listino

Menu **Lavoro sul campo → Listino** (con il modulo Interventi o con il modulo Consuntivi). È
l'**unico catalogo dei prezzi** (manodopera, materiali, trasferta). Interventi, consuntivi e costi dei cantieri leggono il prezzo da qui e lo copiano sulla
riga: se poi il listino cambia, il prezzo storico resta.

- Prezzi **al centesimo**.
- Per la **manodopera a ore** si può indicare una **qualifica** (es. «elettricista»,
  «capocantiere»): la tariffa vale per chi ha quella qualifica.
- **Validità** dal/al: una voce vale solo nel suo periodo.
- La tariffa di una persona in un giorno è quella della sua qualifica valida quel giorno (la più
  alta, se ne ha più d'una); altrimenti la voce di manodopera per tutti.
- Chi lavora non sceglie la propria tariffa. Chi non vede i costi vede «prezzi riservati»: le voci
  che aggiunge prendono il prezzo dal listino.

Le qualifiche delle persone si gestiscono in **Pianificazione → Turni → Qualifiche e competenze**
(modulo Qualifiche e scadenze).

<a id="cap6-cantieri"></a>
### Cantieri e presenze

Menu **Lavoro sul campo → Cantieri**: le commesse di cantiere. Ogni commessa ha codice, nome,
stato (Pianificato, In corso, Sospeso, Chiuso), indirizzo e posizione (con **Trova sulla mappa
dall'indirizzo** e segnaposto da trascinare), committente.

La scheda della commessa ha quattro schede:
- **Avanzamento**: lavorazioni con peso, date, dipendenze e percentuale, diagramma temporale,
  segnalazione «in ritardo»;
- **Persone**: **squadre assegnate** (gruppi di persone di tipo squadra), **presenze**, turni
  collegati;
- **Economia**: budget, costi, **SAL** e **fatture** (solo per chi vede i costi);
- **Diario & Documenti**: il **giornale di cantiere** (nota con data, meteo e numero di presenti)
  e i documenti. Il giornale fa parte dell'unico diario di YW: le note compaiono anche nelle Note di
  lavoro e sono incluse nell'export e nell'anonimizzazione della privacy.

**Presenze (timbrature)**
- **Timbra entrata** / **Timbra uscita** dalla scheda del cantiere.
- Una sola presenza aperta per persona: per entrare in un altro cantiere timbra prima l'uscita. Il
  messaggio dice dove la presenza è ancora aperta.
- Se al cantiere sono assegnate squadre, timbra solo chi ne fa parte (chi pianifica sempre).
- All'entrata YW confronta la posizione del telefono con quella del cantiere: oltre 500 metri la
  presenza vale ma è segnata **«fuori area · da verificare»**. Senza posizione non si verifica.
- Una presenza dimenticata aperta oltre 16 ore viene chiusa d'ufficio **a durata zero** e segnata
  «chiusa d'ufficio · da verificare»: nessuna ora o costo inventato.
- I costi della commessa valorizzano ogni presenza con la tariffa della persona dal listino.

**Fatture di commessa**: numero unico nell'azienda, importo positivo. *Emessa* → *pagata* o
*annullata*, poi lo stato non cambia. Una fattura pagata non si cancella, si annulla.

<a id="cap6-ronde"></a>
### Ronde e cruscotto vigilanza

Menu **Lavoro sul campo → Ronde** (modulo Ronde e punzonature). Un **percorso di ronda** è una
sequenza ordinata di **punti di controllo**; la guardia esegue una **ronda** (un passaggio sul
percorso) e **punzona** ogni punto.

**Preparare un percorso di ronda** (pianificatore)
1. Crea il percorso: nome, **sede** (serve per il fuso orario e per la sede dell'allarme), durata
   massima della ronda in minuti, se ammettere il **codice digitato a mano**, eventuale **gruppo**
   assegnato (solo i membri lo vedono e lo avviano).
2. Aggiungi i punti:

| Tipo | Come si verifica |
|---|---|
| **QR** | il codice scansionato corrisponde. Se non lo inserisci, YW genera un codice sicuro da stampare sull'etichetta |
| **NFC** | il tag avvicinato corrisponde. Codice generato da YW da scrivere sul tag |
| **beacon** | l'identificativo del beacon (obbligatorio) |
| **GPS** | la posizione è entro il raggio dal punto |

Un punto esistente si può riusare in un altro percorso. Un codice troppo corto produce un avviso.

**Regole che rendono affidabili le punzonature**
- La guardia non vede mai il codice atteso.
- Ogni punzonatura registra come è stata fatta (scansione, tag NFC, digitata, automatica) e la
  distanza dal punto.
- A fine ronda diventano **anomalie**: «codice digitato a mano», «punzonato lontano dal punto».
- Punzona solo chi esegue la ronda; il pianificatore vede e chiude le ronde altrui ma non punzona
  per altri.
- Un tentativo fallito non cancella una punzonatura riuscita.
- Una sola ronda in corso per percorso. Una ronda mai chiusa viene chiusa dal sistema come
  **abbandonata** (con anomalia) e il percorso torna avviabile.

Un QR fotografato può essere riscansionato altrove: la distanza registrata lo rende visibile. Per i
punti critici usa NFC o punti GPS.

**Ronde pianificate e ronda mancata.** Gli orari dei piani sono nell'ora locale della sede del percorso
(o dell'azienda: il fuso si imposta in **Sedi e asset**). Se all'orario previsto nessuno avvia la
ronda, si apre l'allarme **Ronda mancata** in Centrale, con escalation ai reperibili. Non è
salva-vita: la manutenzione lo sospende. Un piano si può **sospendere** (nessuna ronda dovuta, nessun
allarme) e **riattivare** dal Cruscotto vigilanza. Creare o riattivare un piano non genera allarmi
arretrati: le ronde dovute partono da quel momento.

**Revisione**: nel dettaglio della ronda, esito per punto, modo di punzonatura, distanza,
tentativi e motivo dell'ultimo fallito.

**Cruscotto vigilanza**: le ronde in corso e gli orari pianificati con l'ora locale; uno slot
mancato ha il simbolo di avviso e apre l'allarme.

<a id="cap6-pianificazione"></a>
### Pianificazione: calendario, agenda tecnici, turni

Menu **Lavoro sul campo → Pianificazione**, con tre schede.

- **Calendario**: turni, interventi, ronde e cantieri in un'unica vista.
- **Agenda tecnici**: gli interventi **da assegnare** e la settimana dei tecnici con il carico di
  ogni giorno; si assegnano persona e orario.
- **Turni** (modulo Forza lavoro e turni):
  - **Turni pianificati**: **+ Pianifica turno**, anche collegato a un intervento o a un cantiere;
    il lavoratore li vede e li conferma dall'app;
  - **Reperibilità**: finestre di reperibilità;
  - **Assenze e indisponibilità**: ferie, malattie; impediscono di pianificare turni in quel
    periodo;
  - **Qualifiche e competenze**: crea le qualifiche e assegnale alle persone (con scadenza). Un turno
    può richiedere una qualifica: lo copre solo chi la possiede, non scaduta;
  - **Copertura minima**: requisiti per giorni e orari, con **verifica copertura** ed **equità**
    della distribuzione.

<a id="cap6-consuntivi"></a>
### Consuntivi

Menu **Lavoro sul campo → Consuntivi** (modulo Ore, spese e consuntivi).

**Il percorso di un consuntivo**: Bozza → In revisione → Da correggere (se rimandato) → Approvato.

1. Chi lavora crea il consuntivo (**+ Nuovo consuntivo**) o lo genera dall'intervento completato.
   Aggiunge righe di ore e spese.
2. Lo **invia** in revisione.
3. Chi approva lo **approva** oppure lo **rimanda** con una nota («Cosa correggere?»).
4. Un consuntivo rimandato si corregge (righe e spese modificabili o eliminabili) e si reinvia.
5. Approvato, non cambia più.

**Regole**
- Nessuno approva il proprio consuntivo (un'azienda piccola può consentirlo nelle impostazioni del
  modulo).
- La **tariffa** non la sceglie chi lavora: viene dal listino (qualifica della persona valida quel
  giorno). Se il listino non ha manodopera, la riga resta *da valorizzare* e la fissa chi approva.
- Il consuntivo dell'intervento si genera solo a lavoro **completato**; se i minuti
  dell'intervento cambiano e il consuntivo è ancora modificabile, la riga si riallinea.
- Minuti da 1 a 1440 per riga, spese maggiori di zero. Importi arrotondati al centesimo.
- Il totale è nella valuta dell'azienda; righe in altre valute restano a parte, mai sommate.
- Chi è limitato a certe sedi vede e approva solo i consuntivi di quelle sedi.
- Lo **storico** (creazione, righe, invio, correzioni con la nota, tariffe, approvazione) è nella
  scheda.

<a id="cap6-ore"></a>
### Ore lavorate

Menu **Lavoro sul campo → Ore lavorate**. È il **registro unico** del tempo lavorato, preso da
quattro fonti, sempre aggiornato:

| Fonte | Cosa conta | Fatturabile |
|---|---|---|
| **Intervento** completato | tempo sull'intervento | sì |
| **Ronda** chiusa | tempo di vigilanza (non le ronde chiuse d'ufficio) | no |
| **Presenza in cantiere** chiusa | tempo sulla commessa (non quelle chiuse d'ufficio) | no |
| **Turno** svolto | presenza in servizio | no |

Lo stesso tempo può essere registrato in più modi (per esempio un intervento durante una presenza
in cantiere): conta **una volta sola** e vince la registrazione più specifica, in quest'ordine:
**intervento › ronda › presenza › turno**. La parte sovrapposta resta visibile come «già contata».

La pagina, per un periodo fino a 62 giorni, mostra:
- **Per persona e giorno**: ore contate, fatturabili, sovrapposte;
- **Turni pianificati e svolti**: ritardo d'inizio, turno senza lavoro registrato;
- **Dettaglio registrazioni**.

Chi approva, pianifica o vede tutti i consuntivi vede tutti; il lavoratore vede solo sé stesso. Le
ore di ronda e i turni non entrano nel consuntivo: il consuntivo resta generato dagli interventi.

---

<a id="cap7"></a>
## 7. Persone e permessi, sedi e asset, privacy e audit

### Persone e permessi
Menu **Configurazione → Persone e permessi** (amministratore o permesso «Amministrazione · Utenti e
ruoli»).

**Utenti**
- **Invita utente**: email e ruolo. La persona riceve un **link di accesso monouso** (valido 7
  giorni) e sceglie lei la propria password (almeno 10 caratteri). Se l'invio email è configurato
  il link parte per email, altrimenti compare da copiare e consegnare. Oltre la licenza compare un
  avviso (le licenze sono indicative: nulla viene bloccato).
- **Cambia password**: ognuno la cambia dal proprio profilo (in basso nel menu), indicando quella
  attuale; le sue altre sessioni aperte si chiudono.
- Scheda della persona: ruolo, **Nuovo link di accesso** (primo accesso non ancora fatto o password
  dimenticata: il link precedente smette di valere), **telefono** (formato internazionale, serve per SMS
  e chiamate) e, per operatori, lavoratori e reperibili, le **sedi visibili** (vuoto = tutte).
- Il telefono dei colleghi lo vede solo chi gestisce persone, allarmi o pianificazione. Chi non
  coordina nessuno vede solo sé stesso nell'elenco.
- Un utente retrocesso ha subito i nuovi permessi; uno eliminato perde la sessione.

**Gruppi di persone**: insiemi di persone riusabili ovunque (squadre, reparti, zone, turni). Un
gruppo ha nome e **tipo**; le **squadre** sono gruppi di tipo «squadra». Si usano per assegnare
interventi, percorsi di ronda e cantieri, per i ruoli personalizzati e, con **Importa in gruppo
di escalation**, per copiarne i membri in un gruppo di escalation (la copia non si svuota se elimini il gruppo di
partenza).

**Ruoli personalizzati**: pacchetti di permessi con un nome (es. «DPO» con solo Privacy e Audit),
assegnabili a persone e gruppi. Aggiungono permessi al ruolo base, non ne tolgono. Valgono subito.

**Permessi**: la tabella ruolo × permesso. Si aggiungono o tolgono permessi a un ruolo per tutta
l'azienda; un pallino segna ciò che è diverso dal predefinito; **ripristina default** torna
all'origine.

| Permesso | Cosa consente |
|---|---|
| Amministrazione completa | tutto |
| Sicurezza · Vedere allarmi | Centrale, mappe, dispositivi, report |
| Sicurezza · Gestire allarmi | prendere in carico, chiudere, annotare, provare gli allarmi |
| Dispositivi · Inviare comandi | comandi ai dispositivi |
| Lavoro sul campo · Pianificare | ordini, interventi, ronde, turni |
| Consuntivi · Approvare / Vedere tutti | approvazione e lettura di tutti i consuntivi |
| Interventi · Vedere tutti | tutti gli interventi delle proprie sedi |
| Costi · Vedere | budget, costi, SAL, fatture, prezzi dei materiali |
| Configurazione · Sola lettura | impostazioni e diagnostica senza modificarle |
| Lavoro personale | i propri interventi, consuntivi, ronde e turni nella dashboard |
| Amministrazione · Utenti e ruoli | persone, gruppi, permessi, ruoli, sedi, asset |
| Amministrazione · Dispositivi | censimento, chiavi, onboarding, aree, piantine |
| IoT · Regole e sensori | regole sensori, monitoraggio e alert IoT |
| Escalation e reperibilità | gruppi, turni di reperibilità, trigger, profili, recapiti |
| Impostazioni del tenant | configurazione guidata, moduli, funzioni AI |
| Privacy (GDPR) | export e anonimizzazione dei dati personali |
| Audit · Registro attività | lettura del registro attività |
| Dashboard · Organizzare | riquadri della dashboard |

Permessi predefiniti per ruolo: l'**operatore** vede e gestisce gli allarmi e invia comandi; il
**reperibile** gestisce gli allarmi; il **responsabile** vede e gestisce gli allarmi, approva, vede
tutti i consuntivi, gli ordini e i costi; il **pianificatore** pianifica; l'**auditor** vede
allarmi, consuntivi, ordini, costi e configurazione; il **lavoratore** accede ai propri dati.

**Regole di sicurezza della delega**
- Chi gestisce le persone senza essere amministratore può dare solo i permessi che ha già: non
  crea amministratori, non si promuove, non modifica chi ha più permessi di lui.
- Non si può retrocedere o eliminare l'ultimo amministratore attivo.

### Sedi e asset
Menu **Configurazione → Sedi e asset**.
- **Sedi**: nome, indirizzo, posizione, fuso orario (vuoto = quello dell'azienda) e **indicazioni
  d'accesso** (visibili solo a pianificatore, amministratore e tecnico assegnato). **Trova sulla
  mappa dall'indirizzo** compila la posizione; trascina il segnaposto per il punto esatto
  (cancello, ingresso). La ricerca dell'indirizzo la fa il tuo browser con OpenStreetMap; se non
  funziona, posiziona a mano.
- **Fuso orario dell'azienda** (predefinito Europa/Roma): vale per le ronde pianificate.
- **Asset / mezzi**: veicoli, attrezzature, strumenti, a cui si possono assegnare dispositivi.

Sedi, persone e asset collegati ad altri dati (allarmi, ordini…) non si possono eliminare finché
sono in uso. Un dispositivo invece si può eliminare del tutto.

### Privacy e audit
Menu **Configurazione → Privacy e audit** (permessi «Privacy (GDPR)» o «Audit · Registro
attività»; di default l'amministratore).

- **Dati personali**: elenco delle persone filtrabile per email, ruolo, stato, con export CSV.
- **Esporta** (diritto d'accesso): scarica un file con tutti i dati della persona: profilo,
  check-in, diario, reperibilità, allarmi, recapiti, gruppi, dispositivi, turni, assenze,
  consuntivi, ronde, presenze, qualifiche, interventi.
- **Anonimizza** (diritto all'oblio): sostituisce email e telefono con valori anonimi, blocca
  l'accesso, toglie la persona da gruppi, reperibilità future e ruoli, libera badge e app,
  cancella le posizioni dell'app personale e il testo del diario. Lo storico di allarmi,
  consuntivi e ronde resta, con un identificativo anonimo. Non si può anonimizzare sé stessi,
  l'ultimo amministratore o chi ha allarmi aperti assegnati (riassegnali prima).
- **Conservazione dei dati**: mostra se è attiva la cancellazione automatica dei dati vecchi e
  dopo quanti giorni. La imposta chi gestisce il server.
- **Registro attività**: chi ha fatto cosa e quando, con filtri per azione, persona, testo e date,
  ed export CSV. Le azioni sono descritte in chiaro (es. «Campo: valore»). Anche gli export sono
  registrati.

---

<a id="cap8"></a>
## 8. Assistente e agente AI

Con il modulo **Assistente YouWorker.ai e diario vocale** l'AI entra nei processi: risponde a
domande sui dati dell'azienda, propone azioni e scrive note e righe. La connessione al fornitore
AI (youworker.ai oppure un fornitore esterno) la imposta il superadmin.

**Attivare le funzioni** — **Moduli e AI → AI** (amministratore). Ogni processo si accende da
solo:

| Processo | Cosa fa |
|---|---|
| Assistente operativo | risposte in linguaggio naturale sui dati dell'azienda |
| Agente AI | analizza e scrive: note di diario, righe di consuntivo, note sugli allarmi, report, bozze di intervento |
| Sintesi incidenti | riassunto e prossime verifiche su un allarme, in Centrale |
| Suggerimento regole sensori | proposte di regole dai dati dei sensori |
| Anomalie check-in | schemi ricorrenti nei check-in mancati |
| Assistenza interventi | promemoria operativo su un intervento |
| Sintesi consuntivi | riepilogo di ore e spese per chi approva |
| Diario vocale | trascrizione dell'audio delle note di lavoro |

I processi che inviano al fornitore AI **dati personali** (segnati «dati personali») partono
**spenti**. Per accenderli l'amministratore legge e conferma un **avviso privacy**: va fatto solo
se il trattamento è coperto dalle informative e dal contratto con il fornitore. La conferma resta
nel registro attività. Il suggerimento delle regole sensori non usa dati personali.

**Usare l'agente** — **Operatività → Agente AI** in console, o **Assistente AI sul campo**
nell'app del lavoratore. Scrivi cosa ti serve; l'agente legge i dati e propone le azioni.
- Le azioni sensibili compaiono come **proposte da confermare**, descritte in chiaro. La conferma
  vale una volta, per te, entro 30 minuti.
- Se l'agente ha letto testi dell'azienda (note, diari, messaggi dei dispositivi), **ogni**
  scrittura diventa una proposta da confermare: un testo letto non può far agire l'agente da solo.
- L'agente rispetta gli stessi permessi e le stesse sedi della console.
- Le azioni sulla catena di sicurezza restano sempre manuali. Spegnere l'AI non toglie mai
  l'inserimento manuale, il check-in o l'SOS.

---

<a id="cap9"></a>
## 9. Superadmin

La console superadmin gestisce tutte le aziende della piattaforma. Ha cinque schede:
**Panoramica**, **Tenant**, **Sistema**, **Scenari IoT**, **Cataloghi**.

### Accesso e verifica in due passaggi
- Ogni account superadmin usa **password e codice** di un'app di autenticazione (verifica in due
  passaggi obbligatoria).
- Al primo accesso con la password si apre solo la pagina di attivazione: inserisci la chiave
  nell'app di autenticazione e conferma con un codice. Da lì servono sempre password e codice. Lo
  stesso codice non vale due volte.
- **Sistema → Account superadmin**: nuovo account (password di almeno 12 caratteri), disattiva o
  riattiva (mai sé stessi, sempre almeno uno attivo), nuova password, **azzeramento della
  verifica** (telefono perso), cambio della propria password. Quando un account viene disattivato
  o cambia password o verifica, le sue sessioni aperte si chiudono.

### Panoramica
Riepilogo: aziende totali, operative, da configurare, sospese, utenti, dispositivi, allarmi
aperti, inviti in sospeso; stato sintetico del sistema e alert recenti.

### Tenant
Elenco compatto delle aziende con stato e prontezza («operativo», «da configurare», «invito in
sospeso»), filtri rapidi e ricerca. Un clic espande la riga.

**Creare un'azienda** (**+ Nuovo tenant**)
1. Nome e **slug** (diventa l'indirizzo `slug.<dominio>`, con anteprima) ed email
   dell'amministratore.
2. **Pacchetto**: attiva in un colpo i moduli di *sicurezza*, *campo*, *IoT* o *completo*.
3. **Licenze** app e IoT (vuoto = illimitato). Sono indicative: oltre il limite nulla si blocca,
   ma si vede l'uso in rosso e l'amministratore riceve un avviso.
4. Facoltativo: uno **scenario IoT** con dati demo.
5. Attivazione dell'amministratore: **invito monouso** (consigliato: ricevi il link da consegnare,
   valido 7 giorni) oppure password diretta (solo prove).

**Dettaglio di un'azienda**
- **Moduli** raggruppati per area (Sicurezza lavoratore, Lavoro sul campo, IoT, Assistente AI):
  un interruttore per modulo, effetto immediato.
- **Amministrazione**: licenze e **identità visiva** (logo PNG, JPEG o WebP fino a 512 KB, colori).
  Il logo dell'azienda prende il posto del suo nome; il logo YouWorkforce resta sempre.
- **Impersona admin**: entri come un amministratore reale dell'azienda, per assistenza. Se non ne
  esiste uno, va creato o reinviato l'invito. Uscendo torni alla tua identità.
- **Strumenti per modulo**: le impostazioni dei moduli dell'azienda.
- **Scenari IoT (casi d'uso)**: attiva o rimuove gli ambienti IoT sull'azienda.
- **Visibilità mappe**: quali mappe (esterne, interne) l'azienda vede.
- **Connessione AI**: fornitore (youworker.ai, OpenAI, DeepSeek, Anthropic, personalizzato),
  indirizzo, modello e chiave (salvata cifrata). Dopo il salvataggio YW verifica che il fornitore
  risponda.
- **Sospendi**: *subito* oppure *alla data* con un motivo (fino ad allora l'amministratore vede un
  avviso). Un'azienda sospesa è ferma del tutto: non riceve né inoltra allarmi, dispositivi e app
  sono ignorati, gli utenti sono disconnessi. **Riattiva** o **Annulla sospensione programmata**
  la riportano attiva; l'escalation riprende per gli allarmi rimasti aperti.
- **Elimina**: cancellazione definitiva di tutti i dati; per confermare si riscrive lo slug.

### Sistema
- **Stato sistema**: server (CPU, memoria, disco), applicazione e database, code e carico, con
  semaforo verde/giallo/rosso. Un controllo automatico apre un alert per ogni problema nuovo, lo
  notifica al superadmin e lo chiude quando rientra; c'è lo **storico alert**. Il controllo gira
  separato dal resto: se il motore di lavoro si blocca, scatta l'alert.
- **Configurazione monitoring**: soglie e destinatari email/SMS degli alert di sistema.
- **Consegna allarmi**: dice se la consegna è **reale**, **di prova** o **simulata** (rosso), e lo
  stato di email e Slack.
- **Canali di consegna degli allarmi**: Twilio per SMS e chiamate (con numero mittente in formato
  internazionale), Slack, relay email. Sono gli unici collegamenti esterni oltre all'AI. Le
  credenziali le imposta solo il superadmin, sono cifrate e non vengono più mostrate dopo il
  salvataggio.
- **Cifratura dei segreti**: la chiave che protegge le chiavi AI, i canali di consegna e i codici
  della verifica in due passaggi. YW la genera da solo e la conserva sigillata dal server.
  - Imposta subito una **passphrase di recupero** (almeno 12 caratteri) e conservala in un posto
    sicuro: serve per sbloccare la chiave su un altro server, dopo un ripristino o una migrazione.
    YouWorkforce non può recuperarla. Finché manca, il controllo è giallo.
  - **Ruota la chiave** per ricifrare i segreti con una chiave nuova.
  - **Chiave bloccata** (server nuovo): sbloccala con la passphrase. Se è persa, **Azzera la
    chiave** (scrivendo AZZERA): le chiavi AI salvate vanno reinserite. Con la chiave bloccata gli
    allarmi funzionano comunque.
- **Dispositivi rifiutati**: badge che provano a collegarsi e vengono rifiutati, con motivo
  (sconosciuto, conteso tra più aziende, modulo spento) e aziende in cui l'IMEI è registrato.

### Scenari IoT
Catalogo degli ambienti IoT (16 predefiniti): crea, modifica, disattiva o elimina. Uno scenario
predefinito si disattiva ma non si elimina; uno attivo su qualche azienda non si elimina; le
modifiche valgono per le nuove attivazioni (per aggiornare un'azienda si riapplica lo scenario).
Rimuovendo uno scenario da un'azienda si elimina solo la sua sede demo, se vuota.

### Cataloghi
- **Pacchetti**: i gruppi di moduli proposti quando crei un'azienda (Sicurezza lavoratore, Lavoro
  sul campo, IoT, Completo e quelli che aggiungi). Per ogni pacchetto scegli nome, descrizione e
  moduli; i moduli di cui questi hanno bisogno si attivano da soli. Un pacchetto disattivato non
  compare più nella creazione delle aziende. I predefiniti non si eliminano. Cambiare un pacchetto
  non tocca le aziende già create.
- **Tipi di evento**: nome, famiglia e spiegazione di ogni tipo di evento (SOS, uomo a terra,
  dispositivo offline, ingresso in area…), uguali in Centrale, mappe, report, messaggi di allarme e
  regole di tutte le aziende. La **famiglia** raggruppa le varianti: per esempio «Dispositivo che
  non comunica» comprende dispositivo offline, perdita contatto, segnale periodico assente e
  dispositivo inattivo. La colonna **Gestione** dice se il tipo è un evento, un allarme critico o
  un allarme salva-vita: lo decide il prodotto e non si modifica da qui.

---

<a id="cap10"></a>
## 10. Domande frequenti e problemi comuni

| Problema | Causa probabile | Cosa fare |
|---|---|---|
| In Centrale c'è l'avviso «Gli allarmi non arrivano ai reperibili» | consegna simulata o di prova | chiedi al superadmin di configurare i canali di consegna |
| Un reperibile non riceve SMS | telefono mancante, non verificato, oppure opt-out | inserisci il telefono in Persone e permessi, verificalo in Escalation → Recapiti; se c'è opt-out fai rispondere START o premi Riabilita e riverifica |
| Arriva l'SMS ma non la chiamata | fallback vocale spento o canale voce non attivo | Escalation → Notifiche → Fallback vocale; verifica i canali con il superadmin |
| Un allarme critico non va a nessuno | nessun profilo, routing né gruppo di default | imposta il gruppo di default e la rete di ultima istanza |
| Gli allarmi dei sensori non vengono inoltrati | manutenzione attiva | Escalation → Notifiche → Termina (i salva-vita passano comunque) |
| La voce Escalation non compare | nessun modulo che produce allarmi è attivo | chiedi al superadmin di attivarne uno |
| Il badge non si collega | IMEI errato, server o porta sbagliati, IMEI conteso, modulo spento | scheda del badge → Collegamento → verifica ora |
| Il badge è assegnato ma non c'è allarme di silenzio | non è stato messo in servizio | scheda del badge → Metti in servizio |
| Nessun allarme di silenzio fuori turno | è impostato «solo durante il turno» | Collegamento dispositivi → Soglie e allarmi |
| L'app non sorveglia il silenzio | manca il modulo Forza lavoro e turni, o il lavoratore non ha iniziato il turno | attiva il modulo; il lavoratore preme Inizia turno |
| L'immobilità non scatta mai | «solo durante il turno» senza modulo Forza lavoro e turni | togli la spunta nel profilo o attiva il modulo |
| Falsi allarmi di silenzio dell'app | il telefono sospende l'app a schermo bloccato | inserisci il telefono del lavoratore (riceve il preavviso SMS); per chi è a rischio usa un badge |
| Il suono dell'allarme in console non parte | il browser blocca l'audio prima del primo clic | fai un clic sulla pagina a inizio turno |
| Nell'app mancano SOS e check-in | modulo Lone Worker non attivo | chiedi al superadmin |
| Non posso timbrare l'entrata in cantiere | hai una presenza aperta altrove, oppure non sei nella squadra assegnata | timbra l'uscita dove indicato; chiedi al pianificatore |
| Presenza «fuori area · da verificare» | eri oltre 500 metri dal cantiere o il GPS era impreciso | il responsabile la verifica; la presenza vale |
| Non posso approvare un consuntivo | è tuo, oppure è di una sede che non ti è assegnata | lo approva un collega autorizzato |
| La riga del consuntivo è «da valorizzare» | il listino non ha la manodopera per quella persona e quel giorno | chi approva fissa la tariffa, o si aggiunge la voce al listino |
| Non vedo i prezzi | non hai il permesso «Costi · Vedere» | chiedi all'amministratore |
| Una funzione AI non si accende | connessione AI assente o avviso privacy non confermato | il superadmin imposta la connessione; l'amministratore conferma l'avviso |
| Accesso bloccato per qualche minuto | troppi tentativi sbagliati | aspetta 5 minuti |
| «Riferimento non valido: il dato indicato non appartiene alla tua azienda» | si è tentato di collegare un dato di un'altra azienda | scegli un dato della tua azienda |

---

<a id="appendice"></a>
## Appendice tecnica

Per chi installa e gestisce il server. I dettagli completi sono nel [README](../README.md) e nei
manuali di area.

### Componenti e porte
- **Core** (Rust): API, motori nativi (escalation, ricezione dispositivi, IoT), migrazioni
  automatiche all'avvio. Porta HTTP predefinita `8080` (`YW_BIND`).
- **Console** (Go): serve console, superadmin e app del lavoratore e inoltra `/api` al core. Porta
  `8081`. Su Windows si avvia `console.exe` dalla cartella `console/`.
- **PostgreSQL 18** con isolamento per azienda nel database (Row Level Security).
- **Teltonika TCP** `5027` (`YW_TELTONIKA_PORT`, `off` per disattivare).
- **MQTT** `1883` (`YW_MQTT_PORT`), MQTT cifrato `8883` (`YW_MQTTS_PORT`) con certificato e chiave
  `YW_MQTT_TLS_CERT`/`YW_MQTT_TLS_KEY`; `YW_MQTT_PLAIN=off` chiude la porta in chiaro;
  `YW_MQTT_MAX_CONN` (predefinito 500).

Avvio rapido: `docker compose up -d --build` (console su `:8081`, core su `:8080`).

### Variabili principali

| Variabile | A cosa serve |
|---|---|
| `YW_DB_OWNER`, `YW_DB_RT` | connessioni al database (proprietario per le migrazioni, ruolo applicativo senza bypass dell'isolamento) |
| `YW_BASE_DOMAIN`, `YW_CONTROL_HOST` | dominio delle aziende (`slug.<dominio>`) e host di controllo |
| `YW_CORE_URL`, `YW_CONSOLE_BIND` | indirizzo del core per la console e porta della console |
| `YW_SUPERADMIN_EMAIL`, `YW_SUPERADMIN_PASS` | creano solo il **primo** account superadmin |
| `YW_SUPERADMIN_MFA=on` | impone la verifica in due passaggi anche in sviluppo |
| `YW_KEYSTORE_DIR` | su Linux, cartella della chiave di macchina (predefinita `/var/lib/youworkforce`, non va scritta a mano) |
| `YW_SECRET_KEY` | solo per convertire segreti di versioni vecchie; dopo la conversione si toglie |
| `YW_COOKIE_SECURE=1` | obbligatoria in HTTPS (staging e produzione) |
| `YW_TRUSTED_PROXIES` | proxy fidati da cui leggere l'indirizzo reale del client |
| `YW_RETENTION_DAYS` | giorni di conservazione dei dati (0 = disattivata, predefinito) |
| `YW_PRESENCE_RADIUS_M` | raggio per le presenze «fuori area» (predefinito 500) |
| `YW_ESC_TWILIO_*`, `YW_ESC_FROM`, `YW_ESC_SLACK_TOKEN`, `YW_ESC_SMTP_RELAY_URL`, `YW_ESC_SINK_URL` | solo valori iniziali dei canali di consegna: il superadmin li sovrascrive da *Sistema* |
| `YW_ESC_PUBLIC_BASE` | indirizzo pubblico per i link assoluti (heartbeat monitor) |
| `YW_EMAIL_DOMAIN` | dominio degli indirizzi «email → allarme» |
| `YW_SYS_*` | soglie predefinite del monitoring di sistema |
| `YW_METRICS_TOKEN` | protegge l'endpoint delle metriche |
| `YW_WORKER_STACK_MB` | memoria di stack dei processi di lavoro (predefinita 32) |
| `YW_SEED_DEMO=1` | dati e accessi dimostrativi: **mai in produzione** |
| `TEST_CLOCK=1` | orologio di test e funzioni di collaudo: **mai in produzione** |

### Sicurezza
- Password con argon2id; sessioni su database con scadenza e uscita lato server; protezione CSRF;
  limite dei tentativi di accesso condiviso tra istanze.
- L'azienda si ricava dal dominio o dalla credenziale del dispositivo, mai dal contenuto dei dati.
  Ogni riferimento tra dati è verificato nel database: un dato di un'altra azienda viene rifiutato.
- Segreti (chiavi AI, canali di consegna, codici di verifica) cifrati con la chiave gestita da YW.
  Conservare la passphrase di recupero.
- Per la produzione: HTTPS davanti a console e core, `YW_COOKIE_SECURE=1`, MQTT su TLS, porta 5027
  protetta da rete privata, VPN o allowlist.
- Più istanze del core possono funzionare insieme (eventi in tempo reale tramite il database).
- La ricerca degli indirizzi la fa il browser dell'utente: il server non invia posizioni a servizi
  esterni.

<a id="per-saperne-di-piu"></a>
### Per saperne di più

| Area | Documento |
|---|---|
| Registro dettagliato delle modifiche | [manuale.md](manuale.md) |
| Interfaccia della console | [console-ui-manuale.md](console-ui-manuale.md) |
| Escalation e reperibilità | [escalation-manuale.md](escalation-manuale.md) |
| Dispositivi GPS e badge | [dispositivi-gps.md](dispositivi-gps.md) |
| IoT nativo | [iot-nativo.md](iot-nativo.md) |
| Ronde e punzonature | [ronde-manuale.md](ronde-manuale.md) |
| Consuntivi | [consuntivi-correttezza.md](consuntivi-correttezza.md) |
| Ore lavorate, squadre, listino | [proposte-campo-ca4-ca5-ca6.md](proposte-campo-ca4-ca5-ca6.md) |
| Moduli e funzioni | [moduli-autonomia.md](moduli-autonomia.md) |
| Permessi e ruoli personalizzati | [capability-layer.md](capability-layer.md) · [custom-roles.md](custom-roles.md) |
| Privacy e audit | [gdpr-privacy-audit.md](gdpr-privacy-audit.md) |
| Sedi e navigazione | [sedi-geocoding-navigazione.md](sedi-geocoding-navigazione.md) |
