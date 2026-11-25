# YouWorkforce — Manuale utente

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
- **Permessi (capability)**: matrice ruolo × capability (`admin`/`plan`/`approve`). Il default
  riproduce i ruoli storici; si possono aggiungere/togliere permessi per il tenant. Vedi
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
Motore di escalation **nativo** (parità con YWAlert). Dettagli: [escalation-manuale](escalation-manuale.md),
[ywalert-manuale](ywalert-manuale.md).

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
  «Silenzio dell'app»), salvo chi ha già un badge in servizio. Il turno nell'app si apre anche con
  il solo modulo Lone Worker.
- **Badge che non si collega**: nella scheda, riquadro **Collegamento** con «verifica ora»: dice se
  il badge è collegato adesso o se viene rifiutato e perché. Il superadmin vede tutti i badge
  rifiutati in *Sistema → Dispositivi rifiutati*.

- **Device Onboarding Kit (DOK)**: crea in un colpo tutto ciò che serve per un dispositivo (o N
  dispositivi via CSV), per tipologia d'uso.
- **Canali di ricezione dati**:
  - **Decodifica nativa**: Teltonika (TCP), smartphone OsmAnd, JSON HTTP. Nessun server esterno.
  - **Edge nativo Teltonika** (TCP Codec8/8E): posizioni, **telemetria IO** e **comandi GPRS**
    direttamente dal tracker. Nel dettaglio device: pannello telemetria IO + invio comando GPRS.
    Vedi [Dispositivi GPS e badge](dispositivi-gps.md).
- **Mappa dispositivi**: posizioni live, geofence, presenza. Condivisa fra Sicurezza e Campo.
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

## 11. Assistente AI

L'AI è **attore** nei processi (multi-provider): agente con strumenti che leggono e **scrivono** sul
sistema (le operazioni sensibili richiedono conferma), report persistiti, diario vocale sul campo.
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

- **Combo con ricerca** ovunque, ricerche full-text (Dispositivi, Centrale), mappe separate,
  tema chiaro/scuro.
- **Lingua IT/EN**: selettore in alto a destra; le schermate chiave sono tradotte, le altre
  ricadono sull'italiano.
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

**Console superadmin a schede.** **Panoramica** (KPI tenant + stato sistema sintetico + alert recenti),
**Tenant** (lista + filtri; il form "Crea tenant" è dietro il bottone **+ Nuovo tenant**), **Sistema**
(monitoring infrastrutturale completo).

**Monitoring sistema + watchdog (scheda Sistema).** `GET /api/superadmin/system` espone in tempo reale:
server (host/OS, CPU%, memoria usata/totale, disco — via `sysinfo`), applicazione (versione, uptime),
database (online, dimensione, connessioni/max), worker (heartbeat), code (outbox in coda/errore,
sessioni) e aggregati (tenant/utenti/dispositivi/incidenti). Ogni metrica ha una **soglia** (env
`YW_SYS_*`, default: disco/mem 90%, CPU 95%, worker >120s) con semaforo verde/giallo/rosso. Un
**watchdog** nel worker (~60s) apre un alert per ogni problema nuovo (dedup 60 min per tipo), lo
**notifica al superadmin** (sink webhook / email relay / SMS Twilio, riuso motore nativo), lo risolve
quando rientra, e lo storicizza in `app.system_alerts` (mig 0121, `GET /api/superadmin/system/alerts`).
Le **soglie e i destinatari** (email/SMS) sono configurabili **dal pannello** (scheda Sistema →
«Configurazione monitoring»): `GET/POST /api/superadmin/system/settings` (tabella `app.system_settings`,
mig 0122); un campo vuoto usa il default env/prodotto. e2e: `qa/system-monitoring-e2e.sh` (9/9).

**Cruscotto superadmin (usabilità con molti tenant).** La home mostra in cima un **riepilogo** a tile
(tenant totali, operativi, da configurare, sospesi, utenti, dispositivi, incidenti aperti, inviti in
sospeso) calcolato da `list_tenants` (che ora espone anche `incidents_open`). Sotto, i tenant sono
**righe compatte** (stato + prontezza + moduli attivi/utenti/dispositivi/licenze) che si **espandono
al click** per mostrare toggle moduli e gestione — così non si scorre una lista infinita di
attivazioni. **Filtri rapidi** (Tutti / Operativi / Da configurare / Sospesi) + ricerca per nome/slug.

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
| Escalation e reperibilità | [escalation-manuale.md](escalation-manuale.md) · [ywalert-manuale.md](ywalert-manuale.md) |
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
