# YouWorkforce — Manuale utente

> **Manuale per chi usa YW ogni giorno: [manuale-utente.md](manuale-utente.md).** Questo file resta il registro dettagliato delle funzioni e delle modifiche, per gli sviluppatori.

Guida unica alle funzionalità della piattaforma. Pubblico: **amministratori di tenant**, **operatori
di centrale**, **pianificatori**, **responsabili**, **tecnici/guardie sul campo**. Per i dettagli
tecnici di ogni area ci sono i manuali specifici, richiamati di volta in volta.

> Accesso: console web del tenant (es. `http://localhost:8081`), app operatore (PWA) su `/worker/`,
> console superadmin per la gestione multi-tenant.

---

## 1. Concetti di base

- **Tenant**: l'organizzazione cliente. Ogni tenant ha i propri utenti, dati e moduli; i dati sono
  isolati (un tenant non vede quelli di un altro).
- **Moduli**: funzionalità attivabili per tenant senza rilascio software. Si accendono/spengono dal
  superadmin. Una voce di menu compare solo se il modulo relativo è attivo.
- **Ruoli**: `tenant_admin`, `operatore`, `pianificatore`, `responsabile`, `lavoratore`,
  `reperibile`, `auditor`. Determinano cosa si può fare.
- **Due grandi aree**:
  - **Sicurezza lavoratore**: centrale allarmi, check-in, dispositivi SOS/man-down, escalation.
  - **Lavoro sul campo**: interventi, ronde, consuntivi, turni. Funziona anche su tenant senza
    Sicurezza (e viceversa).

> **Menu (25/11/2026):** le voci sono state riordinate (27 invece di 41, pagine simili riunite in
> schede: Centrale, Mappa, Pianificazione, Regole di sicurezza, Moduli e AI). Alcuni nomi usati più
> avanti in questo manuale sono quelli precedenti; la corrispondenza è in
> [console-ui-manuale §10-bis](console-ui-manuale.md).

## 2. Accesso e profilo

- Login con email + password sul dominio del tenant.
- **Tentativi di accesso falliti** (tenant e superadmin): dopo **5** errori in 5 minuti dallo stesso
  indirizzo l'accesso a quell'utenza da quell'indirizzo è bloccato per 5 minuti; dopo **20** errori in
  5 minuti sulla stessa utenza, **da qualunque indirizzo**, l'utenza è bloccata per 5 minuti (anche con
  la password giusta, risposta 429 con *Retry-After*). L'indirizzo del client è quello della
  connessione; `X-Forwarded-For` vale solo se arriva da un proxy fidato (la console YW in locale, o gli
  indirizzi in `YW_TRUSTED_PROXIES`) e se ne usa l'ultimo elemento. e2e: `qa/ratelimit-hardening.sh`.
- L'app operatore (PWA «Oggi») è pensata per telefono: turni, ordini, ronde, check-in, diario vocale.
- Il tema chiaro/scuro si cambia dalla topbar (🌙/☀️); la lingua IT/EN in alto a destra
  (vedi §12).

## 3. Utenti, gruppi e permessi

Due pagine distinte (amministratore):

**Utenti e ruoli** (hub identità & accessi):

- **Utenti**: invito, ruolo base, password, telefono, sedi visibili.
  Chi ha sedi assegnate vede e approva solo nelle sue sedi (qualunque ruolo, tranne
  l'amministratore). Ordini di altri, costi e posizioni dei colleghi richiedono i permessi
  «Interventi · Vedere tutti», «Costi · Vedere» o un ruolo di supervisione (vedi
  [capability-layer](capability-layer.md)).
- **Gruppi di persone (condivisi)**: insiemi di persone riusabili da qualunque modulo; si possono
  **importare** nei gruppi di allarme. Vedi [gruppi-condivisi](gruppi-condivisi.md).
- **Permessi (capability)**: matrice ruolo × permesso (19 permessi, da `admin` a
  `dashboard.manage`; `sites.manage` in arrivo). Il default riproduce i ruoli storici; si possono
  aggiungere/togliere permessi per il tenant. Vedi
  [capability-layer](capability-layer.md).
- **Ruoli personalizzati**: bundle di capability definiti dal tenant, assegnabili a utenti e/o
  gruppi (additivi). Vedi [custom-roles](custom-roles.md).

**Utenti e sedi** (anagrafica operativa):

- **Utenti** (stessa lista), **Sedi** (nome + indirizzo/coordinate/indicazioni d'accesso, queste
  ultime visibili solo agli autorizzati), **Squadre** (Workforce), **Asset / mezzi**.

## 4. Sicurezza lavoratore — centrale allarmi

- **Centrale allarmi**: elenco incidenti con **tab** Aperti/Chiusi/Tutti, ricerca full-text,
  paginazione. Gli incidenti ancora aperti lampeggiano; all'arrivo di un nuovo allarme da
  dispositivo scattano **popup + suono**. Presa in carico, chiusura, note.
- **I miei allarmi** (reperibile, responsabile): si aggiorna **da sola** in tempo reale (apertura,
  presa in carico, chiusura) con popup e suono per i nuovi allarmi. Scheda **Miei** = allarmi
  assegnati a te o per cui sei stato avvisato; scheda **Tutti aperti** sempre disponibile, così un
  allarme non si perde se la notifica non è arrivata. e2e: `qa/miei-allarmi-e2e.sh`.
- **Mappa allarmi**: incidenti georeferenziati (indoor/outdoor); cliccando una riga la mappa si
  centra sul pin con tooltip.
- **Trigger di sicurezza**: configurazione degli eventi che generano allarme.
- **Check-in**: l'operatore conferma la presenza entro un conto alla rovescia; se scade, si apre un
  incidente.
- Dettagli UI: [console-ui-manuale](console-ui-manuale.md).

## 5. Escalation e reperibilità

Pagina a **tab**: Gruppi, Routing (evento→gruppo), Notifiche (fallback vocale, calendario
reperibilità, manutenzione), Recapiti (con verifica OTP), Assenze & turni, Monitor (diagnostica).
Motore di escalation **nativo**, unico motore (il motore esterno GoAlert/YWAlert è stato
ritirato). Dettagli: [escalation-manuale](escalation-manuale.md).

## 6. Dispositivi e app

Pagina **Dispositivi & app**: censimento, assegnazione a lavoratore/asset/area, stato di prontezza
(readiness), ricerca full-text, SOS di prova.

**App del lavoratore: SOS e check-in affidabili (SL-5, SL-6, SL-10).**
- Il **SOS** viene salvato sul telefono prima ancora di leggere la posizione e ritentato in automatico
  (anche dopo aver chiuso o ricaricato l'app, o al ritorno della rete) finché la centrale non lo
  riceve; ogni pressione ha una chiave propria, quindi i ritentativi non creano doppioni. Finché non è
  consegnato compare un avviso rosso con il pulsante **Chiama 112**.
- Il **check-in attivo** viene ripreso dal server dopo un ricaricamento: il conto alla rovescia non si
  perde (`GET /api/checkins/mine`).
- **Check-in mancato** e **SOS dall'app** riportano il **lavoratore** e la **sede** (quella del
  check-in, oppure l'unica sede assegnata al lavoratore). Il check-in scaduto, l'incidente e la
  notifica si registrano insieme: un errore a metà non perde l'allarme.
- Un incidente **senza sede** è visibile a **tutti** gli operatori, anche a quelli ristretti per sede.
e2e: `qa/checkin-sos-attribution-e2e.sh`.

**Perdita di contatto: in turno, si chiude da sola, anche per l'app (Q4).**
- Un badge in servizio che tace oltre la soglia apre l'allarme **Perdita di contatto** solo mentre
  il lavoratore è **in turno** (turno aperto nell'app o pianificato). Fuori turno la scheda del
  badge lo dice: «fuori turno · silenzio non sorvegliato». Chi non usa i turni può tornare al
  controllo continuo in *Collegamento dispositivi → Soglie e allarmi*.
- Quando il badge torna in linea l'allarme **si chiude da solo** (esito «rientrato») e chi era
  stato avvisato riceve la chiusura.
- Anche l'**app** è sorvegliata durante il turno (soglia di default 15 minuti, modulo Lone Worker →
  «Silenzio dell'app»), salvo chi ha già un badge in servizio. Il **turno nell'app** esiste solo
  con il modulo Gestione turni (decisione PO 25/11 sera): con il solo Lone Worker l'app non apre
  turni e quindi il suo silenzio non è sorvegliato; restano check-in, SOS e immobilità.
- **Preavviso prima dell'allarme di silenzio dell'app** (decisione PO 25/11 sera, mig `0153`): a
  schermo bloccato il telefono può sospendere l'app web. 2 minuti prima della soglia
  (`app_silence_warn_s`, 0 = nessun preavviso) il lavoratore riceve un SMS «la tua app non dà segni
  di vita: aprila ora»; se la apre, il battito annulla l'allarme. Un SMS per episodio, serve il
  telefono nel profilo. **Roadmap:** app nativa o PWA con notifiche push, per l'uso salva-vita.
- **Badge: silenzio sorvegliato sempre** (default, mig `0150`); «solo durante il turno» è una
  scelta del tenant. Vedi [Dispositivi GPS e badge](dispositivi-gps.md).
- **Badge che non si collega**: nella scheda, riquadro **Collegamento** con «verifica ora»: dice se
  il badge è collegato adesso o se viene rifiutato e perché. Il superadmin vede tutti i badge
  rifiutati in *Sistema → Dispositivi rifiutati*.

- **Gruppo scelto nell'onboarding (SL-11):** con il profilo di protezione il gruppo vale solo
  per quel lavoratore. Senza profilo si instradano al gruppo solo i tipi di allarme che non hanno
  ancora un gruppo. Prima l'onboarding **sovrascriveva** l'instradamento di tutta l'azienda: gli
  SOS di tutti finivano al gruppo del nuovo badge. La guida finale elenca i tipi rimasti invariati.
- **Tipi di allarme instradabili (SL-19):** in *Gruppi di risposta* si possono instradare TUTTI i
  tipi che aprono un allarme (l'elenco arriva dal server; prima la pagina ne mostrava 11 scritti a
  mano e mancavano per esempio gli allarmi di area). La batteria bassa non apre un allarme e non si
  instrada.
- **Gruppo del lavoratore che ha il badge (SL-14):** se un allarme è attribuito a un lavoratore
  (attribuzione all'orario dell'evento), il gruppo di risposta del suo profilo vale per quel
  lavoratore anche quando il badge è registrato a nome di un altro.
- **Centrale, elenco allarmi (SL-22):** gli allarmi aperti ci sono tutti; di quelli chiusi si
  mostrano gli ultimi 300 (prima c'era un unico limite e, con molti chiusi, un aperto vecchio
  poteva non comparire).
- **Orari dal futuro (IO-10):** una posizione o una telemetria con orario oltre 2 minuti nel
  futuro (orologio del dispositivo sbagliato) viene registrata all'ora del server: non resta più
  «ultima» per giorni né falsa i controlli di silenzio.
- **Device Onboarding Kit (DOK)**: crea in un colpo tutto ciò che serve per un dispositivo (o N
  dispositivi via CSV), per tipologia d'uso.
- **Canali di ricezione dati**:
  - **Decodifica nativa**: Teltonika (TCP), smartphone OsmAnd, JSON HTTP. Nessun server esterno.
  - **Edge nativo Teltonika** (TCP Codec8/8E): posizioni, **telemetria IO** e **comandi GPRS**
    direttamente dal tracker. Nel dettaglio device: pannello telemetria IO + invio comando GPRS.
    Vedi [Dispositivi GPS e badge](dispositivi-gps.md).
- **Mappa dispositivi**: posizioni live, geofence, presenza. Condivisa fra Sicurezza e Campo.
  **Allarmi di area (geofence, SL-7):** l'allarme di ingresso o uscita porta il dispositivo e il
  lavoratore reali. Prima la sorgente «geofence» non trovava il badge e l'allarme non diceva chi
  soccorrere. Se l'area ha dispositivi associati, scatta solo per quelli. Gli attraversamenti
  ripetuti al confine confluiscono nell'allarme ancora aperto (una correlazione per area,
  dispositivo e direzione), senza raffiche. e2e `qa/geofence-alarm-e2e.sh` 7/7.
- **Piantine indoor**: posizionamento beacon/dispositivi, allarmi indoor agganciati alle zone.

## 7. Lavoro sul campo — interventi

Modulo **interventi** (ordini di lavoro): visite di riparazione, cantieri. L'ordine ha sede,
istruzioni, checklist, assegnatario.

- **PWA «Oggi»**: il tecnico registra **arrivo** e **avvio**, spunta la **checklist**, scrive
  **report/ore/materiali**, carica **foto** (online), firma e invia; bozza testo offline in coda.
- **Revisione** in console: esecuzione, report, allegati, firma.
- **Chi opera sull'ordine**: avviano, completano e vedono gli allegati solo l'assegnatario, i
  membri del gruppo assegnato e il pianificatore.
- **Intervento chiuso** (completato o annullato): rapporto, firma, materiali, allegati e
  pianificazione non si modificano più (CA-13).
- **Richieste di intervento**: una richiesta si converte in ordine una volta sola (anche con due
  clic contemporanei); una richiesta chiusa non si converte.
- **Fatture di commessa** (Cantieri): numero unico nel tenant, importo positivo; *emessa* →
  *pagata* o *annullata*, poi lo stato non cambia; una fattura pagata non si cancella (si
  annulla); cambi di stato e cancellazioni finiscono nel registro attività. e2e
  `qa/state-guards-e2e.sh` 18/18 (mig `0140`: indice unico sul numero creato se i dati esistenti
  non hanno doppioni).
- Indipendente da Consuntivi/Workforce. Vedi la sezione Campo in
  [console-ui-manuale](console-ui-manuale.md).

## 8. Lavoro sul campo — ronde e punzonature

Modulo **giri**: giri di controllo per guardie/manutentori.

- **Config** (pianificatore): giri con checkpoint ordinati di tipo **QR/NFC/beacon/GPS**, finestra
  temporale facoltativa.
- **Esecuzione** (PWA): avvio ronda, **punzonatura** di ogni checkpoint — scanner **QR/NFC nativo**
  (fotocamera/Web NFC) con fallback manuale, **GPS** per prossimità — progresso e chiusura con
  **compliance** (completato/incompleto).
- **Punzonature affidabili**: la guardia non vede mai il codice atteso; per QR/NFC il codice lo
  genera il sistema; ogni punzonatura registra come è stata fatta (scansione, tag NFC, digitata a
  mano, automatica) e la distanza dal punto; il codice digitato e la punzonatura lontana dal punto
  diventano anomalie a fine ronda; un giro può accettare solo la scansione; punzona solo chi esegue
  la ronda; un tentativo fallito non cancella una punzonatura riuscita.
- **Ronde pianificate**: gli orari sono nell'ora locale della sede (o dell'azienda: *Utenti e sedi →
  🕒 fuso orario*). Se all'orario previsto nessuno avvia la ronda, si apre l'allarme **«Ronda
  mancata»** in Centrale, con escalation ai reperibili; per questo il modulo Ronde include la
  Centrale anche senza Sicurezza Lavoratore.
- **Ronde dimenticate**: una sola ronda in corso per giro; una ronda mai chiusa viene chiusa dal
  sistema come «abbandonata» (con anomalia) e il giro torna avviabile. Le presenze di cantiere
  aperte da oltre 16 ore si chiudono senza ore conteggiate, da verificare.
- **Presenze di cantiere verificate (CA-15, mig `0148`):**
  - una sola presenza aperta per persona: per entrare in un altro cantiere si timbra prima l'uscita
    (il messaggio dice in quale commessa la presenza è ancora aperta). Le presenze parallele già
    presenti sono state chiuse d'ufficio, da verificare;
  - se alla commessa sono assegnate squadre, timbra solo chi ne fa parte (chi pianifica sempre);
  - all'entrata si confronta la posizione del telefono con la sede della commessa: oltre
    `YW_PRESENCE_RADIUS_M` (default 500 m) la presenza vale ma è segnata **«fuori area · da
    verificare»** (un GPS impreciso non blocca il lavoratore). Senza posizione o senza coordinate
    della sede non è verificabile.
- **Revisione** in console: esito per checkpoint, modo di punzonatura, tentativi. Vedi [ronde-manuale](ronde-manuale.md).

## 9. Consuntivi e turni

- **Consuntivi** (modulo consuntivi): ore, spese, approvazione (responsabile).
  Un consuntivo rimandato si corregge (righe e spese modificabili o eliminabili) e conserva lo
  storico delle note. La tariffa non la sceglie chi lavora: viene dal listino (manodopera a ore) o
  la fissa chi approva. Il consuntivo dell'intervento si genera a lavoro completato. Valute diverse
  non si sommano. Vedi [consuntivi-correttezza](consuntivi-correttezza.md).
- **Turni** (modulo workforce): pianificazione, squadre, reperibilità; conferma turni da PWA.

## 10. IoT e scenari

- **IoT (nativo)**: regole sensori parametriche (per tutti i dispositivi o per un dispositivo, modello, sede o scenario; se più regole scattano vale la più grave), telemetria time-series, attributi e comandi/RPC,
  soglie→incidenti, tutto senza server esterni. Vedi [iot-nativo](iot-nativo.md). 
- **Scenari/casi d'uso**: 16 ambienti attivabili dal superadmin su ogni tenant (moduli + regole
  sensori + demo). Generatore di scenari da descrizione/immagini.

## 10-bis. Configurazione guidata

- **Link coerenti con i moduli (TA-12):** ogni passo della checklist porta a una pagina che il
  tenant vede: recapiti e instradamento in *Escalation e reperibilità*, regole dei sensori in
  *Collegamento dispositivi* (anche senza Lone Worker), clienti solo con il modulo Interventi.
  Anche «Cosa fare adesso» in home mostra turni e interventi solo se i rispettivi moduli sono attivi.

## 11. Assistente AI

L'AI è **attore** nei processi (multi-provider): agente con strumenti che leggono e **scrivono** sul
sistema (le operazioni sensibili richiedono conferma), report persistiti, diario vocale sul campo.
**Difesa dai testi malevoli (IO-14, 25/11/2026):** i risultati dei tool arrivano al modello come
dati delimitati (`<dati>…</dati>`) con l'istruzione di non eseguire mai ordini scritti lì dentro.
Inoltre, dopo che l'agente ha letto dati del tenant (note, diari, messaggi dei dispositivi),
**ogni scrittura** (anche una nota di diario) diventa una proposta da confermare: un testo letto
non può far scrivere l'agente da solo. e2e `qa/ai-agent-e2e.sh` 19/19.
**Dati personali e AI (IO-15, decisione PO 25/11/2026):** i processi che mandano al provider AI
dati personali (assistente, agente, sintesi incidente, anomalie dei check-in, assistenza
interventi, sintesi consuntivi, diario vocale) partono **spenti**. L'admin del tenant li accende
uno per uno in *AI · youworker.ai*, dopo un avviso privacy; la conferma è nel registro attività
(`set_ai_feature` con `privacy_ack`). Senza conferma l'API risponde 400. Il suggerimento delle
regole dei sensori (`sensor_suggest`) non usa dati personali e si accende con la connessione.
**Strumenti IoT nativi (IO-18):** `query_iot_alerts` (alert di processo non chiusi) e
`iot_device_telemetry` (ultimi valori di un sensore), solo con il modulo IoT. Tolti gli strumenti
ThingsBoard rimasti nell'elenco (il connettore è ritirato).
Modulo **assistant**.

**Sicurezza dell'agente (IO-2, IO-3, IO-5, IO-6 · mig `0142`, 25/11/2026).**
- **Conferma delle azioni sensibili:** l'agente salva la proposta sul server
  (`app.ai_agent_pending`). La console mostra una descrizione leggibile e conferma **solo l'id**
  (`POST /api/ai/agent/confirm {pending_id}`). La conferma vale una volta, per lo stesso utente,
  entro 30 minuti. Un'azione costruita a mano dal browser non si esegue più.
- **Letture con le regole della Centrale:** incidenti, check-in ed eventi letti dall'agente, e i
  dati usati nei report AI, richiedono il permesso «vedere/gestire allarmi», il modulo giusto e
  rispettano le sedi consentite. I report AI li vede chi li ha creati, oppure chi ha `audit.view`.
- **Regole sensori create dall'agente:** servono `iot.manage` e il modulo IoT. Una regola «uomo a
  terra» si crea solo a mano.
- **Console:** gli oggetti passati ai pulsanti sono codificati per intero (`jattr` in `app.js`) e
  `esc` codifica anche l'apostrofo. Prima un dato con `&quot;`, per esempio nel nome di una commessa
  o in un argomento scelto dal modello, poteva eseguire codice nella pagina (XSS).
- `qa/console-syntax.sh` controlla che ogni script della console sia JavaScript valido. Il
  25/11 un apostrofo non escapato nella scheda dispositivo aveva bloccato tutta la console.

## 12. Console: usabilità, lingua, responsive

- **Stati vuoti guidati (FE-4):** le tabelle vuote mostrano di nuovo titolo, spiegazione e
  azione. Prima una seconda funzione `emptyRow` con lo stesso nome li sostituiva con il numero di
  colonne («3»). Nel superadmin la ricerca dei tenant rispetta di nuovo i filtri per stato.
  `qa/console-syntax.sh` segnala le funzioni definite due volte.

- **Combo con ricerca** ovunque, ricerche full-text (Dispositivi, Centrale), mappe separate,
  tema chiaro/scuro.
- **Lingua IT/EN**: selettore in alto a destra (anche superadmin, app del lavoratore, invito);
  con EN tutta l'interfaccia è tradotta (traduttore comune `i18n.js` + dizionario generato
  `i18n-en.js`, vedi `console/tools/i18n/README.md`); i dati restano come sono.
- **Responsive**: su telefono la barra laterale diventa un menu a scomparsa; tabelle scorrevoli;
  griglie che si impilano.
- Dettagli e novità: [console-ui-manuale](console-ui-manuale.md).

## 13. Amministrazione multi-tenant (superadmin)

Licenze e branding (logo, palette), attivazione moduli e scenari per tenant, impersonazione del
tenant per assistenza. Tutti i motori (escalation, dispositivi, IoT) sono nativi:
non c'è nulla da collegare all'esterno.

- **Logo**: solo PNG, JPEG o WebP fino a 512 KB; il tipo è riconosciuto dal contenuto del file (non
  dal nome o dall'intestazione), SVG e HTML sono rifiutati perché potrebbero eseguire script nella
  console. Il logo è servito con `nosniff` e CSP chiusa.
- **Impersonazione**: si entra solo come un **amministratore reale** del tenant (se manca, `409`:
  crearlo o reinviare l'invito); all'uscita la sessione superadmin torna con la propria identità.
- **Cookie di sessione**: in staging/produzione (HTTPS) impostare `YW_COOKIE_SECURE=1` per
  l'attributo `Secure`; in locale (http://localhost) resta spento.

**Creazione tenant — onboarding admin sicuro.** Il form valida lo **slug** (normalizzato a
`a-z0-9-`, con anteprima dell'indirizzo `slug.<host>`) e l'**email** admin. Per l'attivazione
dell'amministratore ci sono due modalità:
- **Invito monouso (default, produzione)**: non si imposta alcuna password. Il core genera un
  token (salvato solo come **hash**, valido 7 giorni) e restituisce un **link di attivazione** che
  il superadmin consegna all'admin. L'admin apre `/invite.html?token=…`, imposta la **propria**
  password (min 8) e accede. Nessuna password è nota al superadmin. Endpoint: `GET /api/invite/check`,
  `POST /api/invite/accept` (pubblico, monouso). Tabella `app.user_invites` (mig 0119).
- **Password diretta (demo/dev)**: si imposta subito una password (min 8) e l'admin accede senza invito.

Dopo la creazione, un riepilogo mostra l'URL del tenant e (in modalità invito) il link copiabile.

**Provisioning rapido alla creazione (Ondata 2).** Sempre nel form di creazione:
- **Pacchetto**: preset che **attiva i moduli** in un colpo (`sicurezza`, `campo`, `iot`, `full`).
  Catalogo lato server `GET /api/superadmin/plans` (definito nel codice, consumato dall'UI);
  `create_tenant` espande il pacchetto e attiva i soli moduli disponibili.
- **Licenze** (app mobile / IoT) impostabili subito (vuoto = illimitato).
  Le licenze sono **solo indicative** (SA-6, decisione PO): oltre il limite nulla viene bloccato.
  Il superadmin vede l'uso rispetto alla licenza (chip rossa se superata); l'amministratore del
  tenant vede un avviso in console e, creando un utente oltre il limite, un messaggio di attenzione.
  Conteggio: **app mobile** = utenti con ruolo lavoratore o reperibile non anonimizzati (i ruoli
  personalizzati non contano come utenti); **dispositivi** = dispositivi censiti esclusa l'app
  personale del lavoratore. e2e: `qa/license-advisory-e2e.sh`.
- **Scenario / caso d'uso** opzionale con «dati demo»: applica alla creazione uno degli ambienti IoT
  (riusa il motore scenari — attiva i moduli dello scenario + regole sensori + eventuale demo).

e2e: `qa/tenant-invite-e2e.sh` (15/15: invito, validazioni, pacchetto, licenze, scenario+demo).

**Governance tenant (Ondata 3).** Dalla card di ogni tenant:
- **Sospendi / Riattiva** (`POST /api/superadmin/tenants/:id/status`, mig 0120 + 0130). Un tenant
  **sospeso** è fermo del tutto: **non riceve e non inoltra allarmi** (SOS, uomo a terra, perdita di
  contatto, check-in, sensori), dispositivi e app sono ignorati (ingress `403`, connessioni Teltonika
  chiuse), i watchdog e l'escalation lo saltano, gli utenti sono **disconnessi** e il login risponde
  `403`. L'impersonazione superadmin per assistenza resta ammessa. Due modi:
  - **Subito**: effetto immediato.
  - **Alla data** (`at` futuro + `note`): fino ad allora il tenant resta attivo e l'**amministratore del
    tenant vede un avviso** in console con data e motivo; alla data il worker applica la sospensione
    (orologio unico, audit `tenant_status` con `programmata`). *Annulla sospensione programmata* o
    *Riattiva* riportano il tenant attivo. Alla riattivazione l'escalation riprende per gli incidenti
    rimasti aperti.
  e2e: `qa/tenant-suspension-e2e.sh` (15/15).
- **Elimina** (`DELETE /api/superadmin/tenants/:id`): rimozione **definitiva** con cascade su tutti i
  dati (106 FK `ON DELETE CASCADE`); richiede di **riscrivere lo slug** per conferma; audit globale
  prima della cancellazione. Conferme e richiesta dello slug usano finestre interne alla pagina
  (le finestre native del browser sono bloccate nella webview).
- **Ricerca** nella lista tenant (per nome/slug) e **badge di prontezza** per tenant: «operativo»
  (moduli attivi + admin + sede), «da configurare», o «invito in sospeso».
- **Audit** esteso ai cambi modulo (`set_module`) e licenze/branding.

e2e: `qa/tenant-lifecycle-e2e.sh` (10/10: sospendi/riattiva/login, stato in lista, readiness,
elimina con conferma slug + cascade, audit).

**Dettaglio tenant (25/11/2026).** I moduli sono raggruppati per perimetro: *Localizzazione GPS e
badge* sta in Sicurezza lavoratore, *Qualifiche* e *Prevenzione* in Lavoro sul campo (prima
finivano in «Altro»). Messaggi e intestazioni usano il nome del modulo, non il codice. Dopo
l'attivazione o disattivazione di un modulo l'etichetta di stato si aggiorna subito.

**Console superadmin a schede.** **Panoramica** (KPI tenant + stato sistema sintetico + alert recenti),
**Tenant** (lista + filtri; il form "Crea tenant" è dietro il bottone **+ Nuovo tenant**), **Sistema**
(monitoring infrastrutturale completo).

**Monitoring sistema + watchdog (scheda Sistema).** `GET /api/superadmin/system` espone in tempo reale:
server (host/OS, CPU%, memoria usata/totale, disco — via `sysinfo`), applicazione (versione, uptime),
database (online, dimensione, connessioni/max), worker (heartbeat), code (outbox in coda/errore,
sessioni) e aggregati (tenant/utenti/dispositivi/incidenti). Ogni metrica ha una **soglia** (env
`YW_SYS_*`, default: disco/mem 90%, CPU 95%, worker >120s) con semaforo verde/giallo/rosso. Un
**watchdog** (~60s) apre un alert per ogni problema nuovo (dedup 60 min per tipo), lo
**notifica al superadmin** (sink webhook / email relay / SMS Twilio, riuso motore nativo), lo risolve
quando rientra, e lo storicizza in `app.system_alerts` (mig 0121, `GET /api/superadmin/system/alerts`).
Le **soglie e i destinatari** (email/SMS) sono configurabili **dal pannello** (scheda Sistema →
«Configurazione monitoring»): `GET/POST /api/superadmin/system/settings` (tabella `app.system_settings`,
mig 0122); un campo vuoto usa il default env/prodotto. e2e: `qa/system-monitoring-e2e.sh` (9/9).
**Watchdog indipendente (AR-6, 25/11/2026):** il watchdog di sistema gira in un task separato dal
worker: se il worker si blocca, il battito invecchia e parte l'alert «worker» (prima girava dentro
lo stesso ciclo e si fermava con lui). Se il task del worker termina per un errore viene riavviato
dopo 2 s, così i controlli salva-vita non restano fermi.

**Canali di consegna = eccezioni dichiarate (SA-11, decisione PO 25/11/2026).** YouWorkforce non si
collega a servizi esterni, con due sole eccezioni: il provider AI e i **canali che avvisano i
reperibili** fuori dal sistema (Twilio per SMS e chiamate, Slack, email tramite relay). Le
credenziali le imposta **solo il superadmin** in *Sistema → «Canali di consegna degli allarmi»*
(`GET/POST /api/superadmin/channels`, tabella `app.delivery_channels`, mig `0152`): token Twilio,
token Slack e URL del relay sono cifrati con la chiave dei segreti (`enc:v3`), non vengono più
mostrati e non finiscono nel registro attività (solo quale canale è stato impostato o tolto). Le
variabili d'ambiente `YW_ESC_*` restano solo come valore iniziale dell'installazione; il valore
del superadmin vince. Rotazione e azzeramento della chiave trattano anche questi segreti; più
istanze del core ricaricano i canali ogni minuto. e2e `qa/delivery-channels-e2e.sh` 10/10.

**Consegna allarmi: reale, di prova o simulata (SA-3, 25/11/2026).** Prima, senza
configurazione, gli avvisi di escalation finivano solo nel log e nessuno se ne accorgeva. Ora
`delivery::delivery_status()` ricava dalla configurazione il modo di consegna:
- **reale**: SMS e chiamate via Twilio (`YW_ESC_TWILIO_LIVE=1` e credenziali `YW_ESC_TWILIO_*`);
- **prova**: verso il ricevitore di test `YW_ESC_SINK_URL`, non verso i telefoni;
- **simulata**: nessun canale configurato, nessun reperibile viene avvisato.

Email (relay `YW_ESC_SMTP_RELAY_URL`) e Slack (`YW_ESC_SLACK_TOKEN`) sono indicati a parte.
- Scheda **Sistema**: riquadro «Consegna allarmi» e controllo `delivery`. Il controllo è giallo
  se la consegna è di prova; se è simulata è **rosso** con l'orologio reale (giallo con
  TEST_CLOCK). Il watchdog avvisa il superadmin e chiude l'avviso quando la consegna torna reale.
- **Tenant**: chi segue gli allarmi (`incidents.view`/`handle` o `escalation.manage`) vede in
  Centrale, I miei allarmi ed Escalation un avviso «Gli allarmi non arrivano ai reperibili» (o
  «di prova»). L'avviso usa `GET /api/delivery-status`.
- e2e `qa/delivery-status-e2e.sh` (5/5).

**Cruscotto superadmin (usabilità con molti tenant).** La home mostra in cima un **riepilogo** a tile
(tenant totali, operativi, da configurare, sospesi, utenti, dispositivi, incidenti aperti, inviti in
sospeso) calcolato da `list_tenants` (che ora espone anche `incidents_open`). Sotto, i tenant sono
**righe compatte** (stato + prontezza + moduli attivi/utenti/dispositivi/licenze) che si **espandono
al click** per mostrare toggle moduli e gestione — così non si scorre una lista infinita di
attivazioni. **Filtri rapidi** (Tutti / Operativi / Da configurare / Sospesi) + ricerca per nome/slug.

## 13-ter. Sicurezza dei dati tra aziende e dei segreti (25/11/2026)

- **Dati di un'altra azienda (TA-11, CA-19, mig `0144`).** Un riferimento deve puntare a un dato
  della stessa azienda: un utente in un gruppo, un tecnico o una sede su un ordine, la sede di un
  giro, e così via. Prima bastava conoscere l'id di un dato altrui per collegarlo. Ora il database
  lo verifica su tutte le tabelle collegate (88 tabelle) e per ogni via di scrittura. La richiesta
  viene rifiutata con «riferimento non valido: il dato indicato non appartiene alla tua azienda»
  (HTTP 400). e2e `qa/cross-tenant-refs-e2e.sh` 12/12.
- **Controllo più leggero (review 25/11, mig `0147`).** Il trigger legge solo le colonne di
  riferimento (niente copia JSON dell'intera riga, pesante con allegati e piantine) e in modifica
  scatta solo se cambia un riferimento o l'azienda: gli aggiornamenti di posizione dei dispositivi
  non lo eseguono più.
- **Sedi in caso di errore (review 25/11).** Se la lettura delle sedi di un utente fallisce,
  l'accesso si restringe (restano visibili solo gli allarmi senza sede) invece di aprirsi a tutte.
  Un pianificatore ristretto per sede non apre per id gli ordini di altre sedi, neanche
  dall'assistente AI; l'elenco incidenti dell'assistente filtra per sede prima del limite.
- **Email degli utenti (review 25/11).** Formato verificato anche alla creazione da parte
  dell'amministratore: niente spazi, virgolette, `< >` e simili (finiscono in pagine HTML).
- **Registro attività (TA-13).** Ci finiscono anche membri dei gruppi, ruoli personalizzati
  assegnati, sedi e asset, sempre con l'utente reale. Per le azioni del superadmin l'attore è
  `superadmin:<email>`, non più la sola parola «superadmin».
- **Chiave dei segreti, gestita dal superadmin (decisione PO 25/11/2026, mig `0146`).** Niente
  più chiave in un file di testo o in una variabile d'ambiente: la genera YouWorkforce al primo
  avvio. La chiave protegge le chiavi dei provider AI (formato `enc:v3:<n>`) e sta nel database
  (`app.secret_keys`) solo in due forme sigillate:
  - **dal server**: su Windows con DPAPI (chiave di macchina); su Linux con una chiave di
    macchina che il core crea in `YW_KEYSTORE_DIR` (default `/var/lib/youworkforce`, file 0600,
    mai da scrivere a mano). Una copia del database da sola non basta per leggere i segreti;
  - **con la passphrase di recupero** del superadmin (Argon2id, almeno 12 caratteri). Serve per
    sbloccare la chiave su un altro server, dopo un ripristino o una migrazione.
    YouWorkforce non può recuperarla: va conservata in un posto sicuro.
- **Scheda Sistema → «Cifratura dei segreti»** (solo superadmin): stato, chiave e server su cui
  è sigillata, passphrase impostata sì o no, numero di segreti. Da qui si imposta o cambia la
  passphrase (per cambiarla serve quella attuale), si sblocca la chiave su un nuovo server e si
  ruota la chiave (segreti ricifrati; la vecchia chiave resta disattivata finché nessun segreto
  la usa e almeno 5 minuti dopo la rotazione, poi la pulizia automatica la elimina).
  - **Più istanze del core**: ogni minuto ciascuna controlla la chiave attiva nel DB; se un'altra
    l'ha ruotata o sbloccata, la ricarica e ricifra i segreti rimasti indietro.
  - **Chiave persa** (server cambiato e nessuna passphrase, o passphrase dimenticata): con la
    chiave bloccata compare «Azzera la chiave…». Si scrive AZZERA: viene creata una chiave nuova e
    le chiavi dei provider AI salvate finora vengono **cancellate** (vanno reinserite). È
    registrato nel log (`secrets_reset`). Con la chiave attiva l'azzeramento è rifiutato (409).
  - Con la chiave bloccata le funzioni AI risultano non configurate: il testo cifrato non viene
    mai mandato al provider al posto della chiave.
  - Il controllo di sistema `secrets` è **rosso** se la chiave è bloccata: le funzioni AI restano
    senza chiavi, gli allarmi funzionano comunque.
  - È **giallo** finché la passphrase non è impostata.
  - API: `GET /api/superadmin/secrets` e `POST /api/superadmin/secrets/{recovery,unlock,rotate,reset}`.
- La vecchia `YW_SECRET_KEY` non è più richiesta. Se è ancora impostata serve solo a convertire
  i segreti nei formati `enc:v1` e `enc:v2` (e quelli in chiaro), che all'avvio vengono ricifrati
  con la chiave gestita. Dopo la conversione si può togliere.
- e2e `qa/secret-key-e2e.sh` 15/15; unit test `crypto::tests` (3), compreso il sigillo DPAPI.
  Il test imposta sul database di sviluppo la passphrase di prova `e2e-passphrase-di-recupero`.

- **Account superadmin e verifica in due passaggi (SA-12, decisione PO 25/11/2026, mig `0154`).**
  Gli account del piano di controllo si gestiscono in *Sistema → «Account superadmin»*: nuovo
  account (password di almeno 12 caratteri), disattiva/riattiva (mai sé stessi, sempre almeno uno
  attivo), nuova password, azzeramento della verifica (telefono perso), cambio della propria
  password. Le variabili `YW_SUPERADMIN_EMAIL/PASS` servono solo a creare il primo account.
  - **MFA obbligatoria (TOTP, RFC 6238)**: al primo accesso con la password si apre solo la pagina
    di attivazione (chiave da inserire in un'app di autenticazione, poi un codice di conferma);
    da lì in avanti servono password **e** codice. Lo stesso codice non vale due volte. Il segreto
    è cifrato con la chiave dei segreti; rotazione e azzeramento della chiave ne tengono conto.
  - Se su un server nuovo la chiave dei segreti è bloccata, il codice non si può verificare:
    password + passphrase di recupero permettono solo di sbloccarla, poi si rientra con il codice.
  - Account disattivato, password cambiata o MFA azzerata: le sessioni aperte di quell'account si
    chiudono. Tutto nel registro attività (`superadmin_*`).
  - In sviluppo (`YW_SEED_DEMO=1`) l'MFA non è imposta, per i test automatici, salvo
    `YW_SUPERADMIN_MFA=on`; un account che l'ha attivata la usa sempre. e2e
    `qa/superadmin-mfa-e2e.sh` 20/20, unit test TOTP con i vettori dell'RFC.

## 13-quater. Decisioni PO del 26/11/2026 (prima parte)

- **Inviti (TA-8).** «Invita utente» crea l'utente senza password e genera un invito monouso (7
  giorni, `POST /api/users`, senza `password`); `POST /api/users/:id/invite` dà un nuovo link (gli
  inviti precedenti non usati decadono); `POST /api/me/password` cambia la propria password
  (attuale + nuova di almeno 10 caratteri, chiude le altre sessioni). Le password scelte dalle
  persone (invito, cambio) hanno almeno 10 caratteri; quella iniziale passata dall'API (importazioni,
  test) almeno 8. L'invito parte per email solo se c'è il relay email e `YW_ESC_PUBLIC_BASE`.
  e2e `qa/user-invite-e2e.sh` 13/13.
- **Sedi e asset (TA-17).** Nuovo permesso `sites.manage` (di default pianificatore; l'admin li ha
  tutti); vale ancora `users.manage` per chi lo aveva.
- **Moduli (AR-3, AR-5, mig `0159`).** Due soli stati, attivo e non attivo. Un solo percorso di
  attivazione (pannello superadmin, creazione del tenant, scenari) che attiva anche le dipendenze
  dichiarate; dal pannello si chiede conferma (409 «serve attivare anche…» finché non si conferma).
  Non si spegne un modulo finché un altro attivo ne ha bisogno.
- **Dispositivi (AR-4).** Anagrafica dispositivi, piantine, beacon e aree sono disponibili solo
  con un modulo che li usa (Lone Worker, GPS, IoT, Ronde, Interventi, Cantieri, Forza lavoro):
  riga `devices` della matrice in `modules::FEATURE_MODULES`.
- **Badge Teltonika (SL-16).** Allarme solo quando l'IO dell'evento è attivo (il record del rilascio
  del pulsante non apre un secondo allarme); un record con posizione 0,0 (nessun fix GPS) conta
  come contatto ma non come posizione; la mappa IO → allarme si rilegge ogni minuto. Da verificare
  con il GH-5200 reale.
- **Invio reale (escalation).** SMS e chiamate partono con Twilio solo se le credenziali ci sono
  **e** l'interruttore «Invio reale» è acceso, come mostra lo stato della consegna (prima bastavano
  le credenziali, e la console diceva «di prova» mentre gli SMS partivano).
- **Ruoli personalizzati.** Per eliminare un ruolo bisogna avere tutti i suoi permessi (come per
  modificarlo o assegnarlo).
- **Ripetizione delle notifiche.** Il campo «Ripetizione notifica SOS» dei Trigger non aveva effetto
  ed è stato tolto: le ripetizioni si impostano nella policy di ogni gruppo.
- **Membri dai gruppi di persone (TA-7, mig `0160`).** Gruppi di escalation e profili di protezione
  possono collegare uno o più gruppi di persone (`alarm_group_sources`, `protection_profile_sources`;
  API `POST/DELETE /api/groups/:id/sources[/:ug]` e `/api/safety/profiles/:id/sources[/:ug]`,
  permesso `escalation.manage`). Trigger nel DB tengono i membri sempre allineati
  (`via_user_group` sul membro; quelli a mano hanno NULL e non vengono toccati); nei gruppi di
  escalation i nuovi entrano in coda alla rotazione; nei profili chi ha già un profilo resta lì.
  Togliere a mano un membro arrivato da un gruppo dà 409 con spiegazione. e2e
  `qa/member-sources-e2e.sh`.
- **Catalogo dei tipi di evento (SL-15, mig `0161`).** `app.event_types` è l'unica fonte di nome,
  famiglia (soccorso, connessione, area, dispositivo, veicolo, sensori, ronde, altro) e spiegazione
  di ogni codice evento. Il core lo tiene in memoria (`catalogs.rs`, caricato all'avvio e dopo ogni
  modifica; `devices::etype_label` lo usa, con i nomi predefiniti di riserva); la console lo legge
  da `GET /api/event-types`. Il superadmin lo modifica (`PUT /api/superadmin/event-types/:code`,
  scheda Cataloghi). Critico e salva-vita NON sono nel catalogo: restano `api::CRITICAL` /
  `LIFE_SAFETY` (più i codici `*_critico`) e vengono solo mostrati, così un allarme salva-vita non
  si declassa da un'interfaccia. Le quattro forme di «non comunica» (device_offline,
  perdita_contatto, heartbeat_down, dispositivo_inattivo) e quelle di «area» hanno una famiglia
  comune; i codici non cambiano. Con più istanze del core il nome nuovo arriva alle altre al loro
  riavvio.
- **Catalogo dei pacchetti (SA-10, mig `0161`).** `app.plans_catalog` sostituisce l'elenco fisso nel
  codice: `GET/POST /api/superadmin/plans`, `PUT/DELETE /api/superadmin/plans/:code`,
  `GET /api/superadmin/modules-catalog`. I moduli devono esistere nel catalogo moduli; i
  predefiniti si disattivano ma non si eliminano; creare un tenant con un pacchetto disattivato o
  inesistente dà 400 (prima veniva ignorato in silenzio). e2e `qa/catalogs-e2e.sh`.
- **Un solo diario e manutenzioni automatiche (CA-20, mig `0162`).** Il giornale di cantiere è
  confluito in `app.diary_entries` (colonne `commessa_id`, `diary_date`, `weather`,
  `present_count`; `source='cantiere'`; `user_id` ora può essere vuoto per le note di un utente
  eliminato); `app.commessa_diario` è stata copiata ed eliminata. Le API del giornale
  (`POST /api/commesse/:id/diario`, `DELETE /api/commessa-diario/:id`) restano e cancellano solo
  note di cantiere. I piani di manutenzione hanno `auto_generate` (default sì) e `lead_days`
  (0–90, default 7): il worker (`field::maintenance_autogen`, ~10 min) crea l'intervento quando
  `next_date - lead_days <= oggi` nel fuso dell'azienda; «Genera» e il worker usano la stessa
  funzione (`plan_generate_one`, ora 9:00 locale invece che UTC; piano arretrato: un intervento e
  data portata alla prima scadenza futura; `FOR UPDATE SKIP LOCKED` contro i doppioni tra istanze).
  Hook di test `POST /api/_test/maintenance-autogen` (solo TEST_CLOCK). e2e
  `qa/diary-maintenance-e2e.sh`.
- **Scenari dalla pipeline vera (IO-7).** La demo di uno scenario
  (`POST /api/superadmin/tenants/:id/scenarios/:code/demo`) invia i campioni con
  `persist_telemetry` ai dispositivi demo (serie, ultimo stato, regole, alert IoT; incidente di
  sicurezza solo con Lone Worker attivo, come per un sensore vero). Senza modulo IoT o senza
  dispositivi demo: 409 con spiegazione. Risposta `events`/`alerts`/`incidents`. L'aerazione
  (`POST /api/scenarios/:code/aeration`) crea un comando `set_aeration` {"on":…} in
  `iot_commands`; un dispositivo reale lo prende col pull, quello demo (source `scenario`) viene
  eseguito dal core che risponde con la telemetria `aeration`. Non si scrive più `devices.config`.
  Il cruscotto dello scenario legge la telemetria di tutti i dispositivi (anche demo) e mostra come
  allarmi gli alert IoT (più gli incidenti storici della vecchia demo); i valori sì/no salvati come
  testo tornano booleani. e2e `qa/scenario-pipeline-e2e.sh`.
- **Allarmi da telemetria legati al dispositivo.** L'incidente aperto da una soglia (con Lone Worker
  attivo) cercava il dispositivo con sorgente `iot`: un sensore registrato con altra sorgente
  (demo, LoRaWAN, MQTT…) restava senza dispositivo, quindi senza sede, lavoratore assegnato e
  instradamento per profilo. Ora `Normalized.device_id` porta il dispositivo già noto.
- **Cruscotto per permessi (IO-16).** Widget filtrati per modulo + permesso (non più per ruolo);
  nuovo widget «Dispositivi IoT» (`/api/iot/monitor`); nuovo permesso `work.own` (lavoratore). Vedi
  [capability-layer](capability-layer.md). e2e `qa/dashboard-caps-e2e.sh`.
- **Correzioni dalla revisione dei manuali di ronde e consuntivi.** Modificare la propria riga di
  consuntivo senza indicare la tariffa non la cancella più (prima la console la azzerava a chi
  approva o vede i costi). Nell'app del lavoratore una ronda lasciata con «Torna all'elenco» si
  riprende (prima restava aperta sul server e il percorso dava 409). Nome del modulo nel registro
  `modules.rs`: «Ronde e vigilanza» (come nel DB). Mig `0163`: spiegazione di «Ronda mancata»
  senza «giro».
- **Avvisi personali ritardati (escalation).** Le regole di notifica personali con ritardo degli step
  già sparati continuano fino alla presa in carico anche quando l'escalation passa allo step
  successivo (`oncall::esc_deliver_earlier_rungs`; orario di ogni step dalla cronologia
  `esc_delivery`; idempotenza di `esc_delivery_log`). Prima una «chiamata dopo 5 minuti» veniva
  saltata se lo step aspettava meno. e2e `qa/escalation-personal-delay-e2e.sh`.
- **Ora locale e piccole correzioni del Campo (26/11/2026).** Registro ore (`/api/work-hours`):
  periodo e giorni nel fuso dell'azienda, non in UTC (`field::local_midnight_utc`). Costo di
  manodopera del cantiere: tariffa del giorno locale della presenza. Messaggio «ronda già in
  corso» e nota della ronda abbandonata con l'ora della sede (`field::route_tz`). Listino
  disponibile con Interventi **o** Consuntivi (`PRICE_MODS`; anche la voce di menu). Modifica di un
  percorso di ronda: nome vuoto rifiutato (400). e2e `qa/field-local-time-e2e.sh`;
  `listino-qualifica-e2e` non dipende più dall'ora in cui gira.
- **Campo Tenant nella pagina di accesso.** Con l'azienda presa dal sottodominio (`acme.…`) il
  campo era in sola lettura senza spiegazione e sembrava rotto. Ora è modificabile: un nome
  diverso apre l'indirizzo di quell'azienda (console e app del lavoratore; cache dell'app `yw-shell-v8`).
- **Piani di ronda sospendibili (mig `0164`).** `POST /api/patrol-schedules/:id` `{active}`; alla
  riattivazione le ronde dovute ripartono da adesso (`active_since`, usato da watchdog e cruscotto:
  niente ronde mancate arretrate del periodo sospeso né prima della creazione del piano).
  Eliminare o sospendere un piano inesistente: 404. `dwell_max_min` è riservato e non usato (con
  una sola punzonatura per punto la permanenza non si misura). Il tempo di ogni step di escalation
  nella cronologia (`fired_at`) è quello del core, come lo stato dell'escalation. e2e
  `qa/patrol-schedule-suspend-e2e.sh`.
- **Login con Autentika (mig `0172`), accanto al login con password.**
  - Per tenant, dal superadmin: issuer, client e segreto cifrato, indirizzo di ritorno, ruoli.
  - Flusso Authorization Code + PKCE S256 con YW come BFF; verifica ES256 dell'id_token.
  - Utente per `(tenant, sub)`: collegato per email verificata o creato al primo accesso.
  - Back-channel logout e logout verso Autentika. Bottone «Login con Autentika» sulla pagina di
    accesso.
  - Dettaglio in [autentika-integrazione.md](autentika-integrazione.md); e2e
    `qa/autentika-login-e2e.sh` con finto Autentika `qa/oidc_mock.py`.
- **Fase 6 Pilota della pianificazione automatica (mig `0171`).**
  - Squadre, inizio, settimane e confronto con le settimane prima.
  - Misure del piano: accettati senza modifiche, spostamenti per intervento, straordinario, extra entro
    lo SLA del contratto, ore di pianificazione dichiarate.
  - Obiettivi con esito; un solo pilota attivo; proposte limitate alle squadre del pilota.
  - Decisione finale «estendi» o «concludi» con il motivo.
  - Riquadro Pilota in Pianificazione → Automatica. Dettaglio in
    [pianificazione-automatica.md](pianificazione-automatica.md); e2e `qa/pilot-e2e.sh`.
  - Corretto nel motore: con le squadre scelte e «ripianifica» prendeva anche gli interventi delle
    altre squadre.
- **Fase 5 Pianificazione automatica (mig `0170`, modulo `pianificazione`).**
  - **Motore di base nativo** `yw-base-1`: inserimento al costo minimo per squadra × giorno ×
    posizione, poi miglioramento togli-e-reinserisci. Rispetta patentini, persone presenti, mezzi
    liberi, finestre delle sedi, giornata e straordinario; evita il meteo; riduce gli spostamenti
    (linea d'aria × fattore / velocità).
  - **Bozza da rivedere e applicare**, con ricontrollo di ogni voce: versione cambiata, vincolo
    rigido, mezzi riprenotati insieme.
  - **Interventi fissati** e **contratto dati** `yw-planning/1` per motori esterni (Youco), con la
    loro proposta validata uguale.
  - **Prova sullo storico** in sola lettura. Idealverde, 4 settimane: 0 vincoli violati contro 35,
    spostamenti −18,5%, straordinario 249 contro 3.356 min.
  - Pagina **Pianificazione → Automatica**. Dettaglio in
    [pianificazione-automatica.md](pianificazione-automatica.md); e2e `qa/planning-e2e.sh`.
- **Fase 4b Meteo da MET Norway (mig `0169`).** Fonte delle previsioni scelta per tenant: sistema
  collegato (chiave di integrazione) o **MET Norway** (api.met.no, gratuito, CC BY 4.0), scaricato da
  YW per zona ogni ~3 ore con cache, 304 e User-Agent come da termini di servizio (`YW_METNO_CONTACT`);
  ciclo separato dal worker; stato per zona, «Aggiorna ora» e citazione della fonte nella pagina Meteo.
  Unica connessione esterna oltre all'AI (decisione PO). Dettaglio in
  [meteo-operativo.md](meteo-operativo.md); e2e `qa/metno-e2e.sh` con finto MET `qa/metno_mock.py`.
- **Fase 4 Meteo operativo (mig `0168`, modulo `meteo`).** Le previsioni le scrive Youco (o un altro
  sistema autorizzato) con una chiave di integrazione: zone con posizione (ogni sede usa la zona
  fissata o la più vicina) e fasce orarie di pioggia, probabilità, vento, raffiche e temperatura; YW
  non chiama servizi meteo. Limiti meteo sul tipo di attività (pioggia, vento, temperature, ore
  asciutte dopo) → **avvisi** (mai blocchi) nel controllo dell'intervento; interventi a rischio con il
  primo giorno buono dentro la previsione e «Sposta». Pagina **Lavoro sul campo → Meteo**; avviso in
  Agenda tecnici. Dettaglio in [meteo-operativo.md](meteo-operativo.md); e2e `qa/weather-e2e.sh`.
- **Fase 3 Analisi e stime (mig `0167`, modulo `analisi`).** Origine (`manuale`, `piano`, `richiesta`,
  `integrazione`) ed **extra** di ogni intervento; durate reali per tipo, sede e squadra; **stima dallo
  storico** (sede → squadra → tipo → standard, scalata sulla quantità) proposta nel controllo
  dell'intervento; **precisione** misurata sul passato contro la durata standard; ricorrenze reali dei
  piani; extra per mese, cliente e sede; piano contro realtà. Pagina **Lavoro sul campo → Analisi**.
  Dettaglio in [analisi-stime.md](analisi-stime.md); e2e `qa/analytics-e2e.sh`.
- **Fase 2 Integrazioni (mig `0166`).** Chiavi di integrazione per gestionale, software turni o agente
  (Youco/MCP): agiscono per conto di chi le crea con i permessi scelti, si mostrano una volta, si
  revocano; codice esterno (`external_ref`) su clienti, sedi, commesse, interventi, persone, mezzi,
  tipi di attività e gruppi con `PUT /api/integration/{entità}/{ref}` idempotente (interventi con il
  controllo dei vincoli); `GET /api/integration/changes` per riportare esiti e consuntivi; import CSV.
  Pagina **Configurazione → Integrazioni**. Dettaglio in [integrazioni.md](integrazioni.md); e2e
  `qa/integrations-e2e.sh`.
- **Fase 1 Lavoro sul campo: tipi di attività, mezzi, qualifiche e vincoli (mig `0165`).** Catalogo dei
  tipi di attività (durata base + per unità, persone, qualifiche e mezzi richiesti), finestre orarie
  delle sedi, nuovo modulo **Mezzi e attrezzature** (stato, scadenze, prenotazioni senza
  sovrapposizioni), scadenziario di qualifiche e mezzi, controllo dei vincoli su creazione, modifica e
  assegnazione degli interventi (duri = 409 salvo «Forza»). Dettaglio in
  [lavoro-campo-fase1.md](lavoro-campo-fase1.md); e2e `qa/field-constraints-e2e.sh`.
- **Interventi in quantità (caso reale Idealverde: 60+ giardinieri in squadre).** `GET /api/orders`
  accetta filtri lato server: `state=open|closed`, `from`/`to` (inizio programmato, RFC 3339), `id`,
  `limit` (1..5000); senza parametri l'elenco resta completo. La console carica gli aperti
  (agenda, da pianificare), i chiusi solo nella loro scheda (ultimi 500), la settimana mostrata
  nell'Agenda tecnici e il singolo intervento nella scheda; l'app del lavoratore solo gli aperti
  (cache `yw-shell-v10`). Agenda tecnici per squadra e persona con giorno locale (prima UTC);
  calendario con squadra (`group_id`, nome e colore per squadra), filtro squadra/persona e tipo,
  annullati esclusi; il calendario personale e l'iCal del membro includono gli interventi della sua
  squadra (prima no). Riquadro «Interventi» della dashboard: aperti e prossimi, con la squadra (prima
  «non assegnato»). e2e `qa/orders-filters-e2e.sh`.
- **Interfaccia in inglese completa.** Traduttore comune `console/static/i18n.js` (frasi esatte,
  modelli con parti variabili, forme composte; osserva il DOM e gli attributi) e dizionario
  generato `console/static/i18n-en.js` da `console/tools/i18n/en.json` (estrattore dalle pagine e
  dai messaggi del core). Selettore IT/EN anche in superadmin, app del lavoratore e invito; date,
  ore e importi nella lingua scelta; cache dell'app `yw-shell-v9`. I messaggi del core che usavano
  ancora «gruppo allarme», «gruppo on-call», «override» (e «On-call» in iCal/Slack) seguono il
  glossario.
- **Piano di ronda con «dalle» = «alle».** Era letto come fascia di 24 ore e apriva ronde
  mancate per tutto il giorno: ora è una sola ronda a quell'ora. Il piano nasce con l'orologio
  del core (creazione e decorrenza), come il controllo delle ronde mancate. Era la causa del
  fallimento intermittente di `qa/patrol-missed-e2e.sh` nella regressione completa.
- **Glossario unico (AR-7).** [glossario](glossario.md): gruppo di persone / gruppo di escalation;
  turno pianificato / in servizio / di reperibilità; percorso di ronda e ronda (mai «giro»);
  reperibile (non «on-call»), sostituzione (non «override»). Solo testi di console, messaggi del
  core e manuali: API e tabelle invariate. `qa/glossary-check.sh` (in run-all) blocca i nomi vecchi.

## 13-bis. Privacy e audit (GDPR + registro attività)

Voce menu **«Privacy e audit»** (Configurazione, solo `tenant_admin`): export dei dati personali
di una persona (diritto d'accesso, download JSON), anonimizzazione (diritto all'oblio: scrubba la
PII e blocca il login, preservando lo storico incidenti), stato della policy di ritenzione
(`YW_RETENTION_DAYS`) e **registro attività** (audit log) filtrabile per azione e attore.
Vedi [gdpr-privacy-audit](gdpr-privacy-audit.md).

## 14. Riferimenti rapidi (manuali di dettaglio)

| Area | Manuale |
|---|---|
| Console (UI, ricerca, mappe, tema, responsive, i18n) | [console-ui-manuale.md](console-ui-manuale.md) |
| Escalation e reperibilità | [escalation-manuale.md](escalation-manuale.md) (storico: [archivio/ywalert-manuale.md](archivio/ywalert-manuale.md)) |
| Dispositivi GPS e badge | [dispositivi-gps.md](dispositivi-gps.md) |
| IoT nativo (telemetria/attributi/comandi/regole) | [iot-nativo.md](iot-nativo.md) |
| Gruppi di persone condivisi | [gruppi-condivisi.md](gruppi-condivisi.md) |
| Permessi (capability) | [capability-layer.md](capability-layer.md) |
| Ruoli personalizzati | [custom-roles.md](custom-roles.md) |
| Ronde e punzonature | [ronde-manuale.md](ronde-manuale.md) |
| Assegnazione Campo a gruppi (ordini/giri) | [campo-gruppi.md](campo-gruppi.md) |
| Privacy e audit (GDPR, retention, audit log) | [gdpr-privacy-audit.md](gdpr-privacy-audit.md) |

> Questo manuale è l'indice operativo unico; ogni manuale di dettaglio resta la fonte autorevole
> per la sua area e viene aggiornato insieme al codice.
