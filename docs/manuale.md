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
- L'app operatore (PWA «Oggi») è pensata per telefono: turni, ordini, ronde, check-in, diario vocale.
- Il tema chiaro/scuro si cambia dalla topbar (🌙/☀️); la lingua IT/EN in alto a destra
  (vedi §12).

## 3. Utenti, gruppi e permessi

Pagina **Utenti e sedi** (amministratore):

- **Utenti**: invito, ruolo, recapiti.
- **Sedi**: nome e, facoltativi, indirizzo/coordinate/indicazioni d'accesso (queste ultime visibili
  solo agli autorizzati).
- **Squadre**: raggruppamenti operativi (Workforce).
- **Gruppi di persone (condivisi)**: insiemi di persone riusabili da qualunque modulo; si possono
  **importare** nei gruppi di allarme. Vedi [gruppi-condivisi](gruppi-condivisi.md).
- **Permessi (capability)**: matrice ruolo × capability (`admin`/`plan`/`approve`). Il default
  riproduce i ruoli storici; si possono aggiungere/togliere permessi per il tenant. Vedi
  [capability-layer](capability-layer.md).
- **Ruoli personalizzati**: bundle di capability definiti dal tenant, assegnabili a utenti e/o
  gruppi (additivi). Vedi [custom-roles](custom-roles.md).

## 4. Sicurezza lavoratore — centrale allarmi

- **Centrale allarmi**: elenco incidenti con **tab** Aperti/Chiusi/Tutti, ricerca full-text,
  paginazione. Gli incidenti ancora aperti lampeggiano; all'arrivo di un nuovo allarme da
  dispositivo scattano **popup + suono**. Presa in carico, chiusura, note.
- **Mappa allarmi**: incidenti georeferenziati (indoor/outdoor); cliccando una riga la mappa si
  centra sul pin con tooltip.
- **Trigger di sicurezza**: configurazione degli eventi che generano allarme.
- **Check-in**: l'operatore conferma la presenza entro un conto alla rovescia; se scade, si apre un
  incidente.
- Dettagli UI: [console-ui-manuale](console-ui-manuale.md).

## 5. Escalation e reperibilità

Pagina a **tab**: Gruppi, Routing (evento→gruppo), Notifiche (fallback vocale, calendario
reperibilità, manutenzione), Recapiti (con verifica OTP), Assenze & turni, Monitor (diagnostica).
Motore di escalation **nativo** (parità con GoAlert). Dettagli: [escalation-manuale](escalation-manuale.md),
[goalert-manuale](goalert-manuale.md).

## 6. Dispositivi e app

Pagina **Dispositivi & app**: censimento, assegnazione a lavoratore/asset/area, stato di prontezza
(readiness), ricerca full-text, SOS di prova.

- **Device Onboarding Kit (DOK)**: crea in un colpo tutto ciò che serve per un dispositivo (o N
  dispositivi via CSV), per tipologia d'uso.
- **Canali di ricezione dati**:
  - **Traccar** (edge di decodifica) e **ingest nativo OsmAnd/HTTP** (senza server Traccar).
  - **Edge nativo Teltonika** (TCP Codec8/8E): posizioni, **telemetria IO** e **comandi GPRS**
    direttamente dal tracker. Nel dettaglio device: pannello telemetria IO + invio comando GPRS.
    Vedi [teltonika-edge](teltonika-edge.md).
- **Mappa dispositivi**: posizioni live, geofence, presenza. Condivisa fra Sicurezza e Campo.
- **Piantine indoor**: posizionamento beacon/dispositivi, allarmi indoor agganciati alle zone.

## 7. Lavoro sul campo — interventi

Modulo **interventi** (ordini di lavoro): visite di riparazione, cantieri. L'ordine ha sede,
istruzioni, checklist, assegnatario.

- **PWA «Oggi»**: il tecnico registra **arrivo** e **avvio**, spunta la **checklist**, scrive
  **report/ore/materiali**, carica **foto** (online), firma e invia; bozza testo offline in coda.
- **Revisione** in console: esecuzione, report, allegati, firma.
- Indipendente da Consuntivi/Workforce. Vedi la sezione Campo in
  [console-ui-manuale](console-ui-manuale.md).

## 8. Lavoro sul campo — ronde e punzonature

Modulo **giri**: giri di controllo per guardie/manutentori.

- **Config** (pianificatore): giri con checkpoint ordinati di tipo **QR/NFC/beacon/GPS**, finestra
  temporale facoltativa.
- **Esecuzione** (PWA): avvio ronda, **punzonatura** di ogni checkpoint — scanner **QR/NFC nativo**
  (fotocamera/Web NFC) con fallback manuale, **GPS** per prossimità — progresso e chiusura con
  **compliance** (completato/incompleto).
- **Revisione** in console: esito per checkpoint. Vedi [ronde-manuale](ronde-manuale.md).

## 9. Consuntivi e turni

- **Consuntivi** (modulo consuntivi): ore, spese, approvazione (responsabile).
- **Turni** (modulo workforce): pianificazione, squadre, reperibilità; conferma turni da PWA.

## 10. IoT e scenari

- **IoT/ThingsBoard**: regole sensori parametriche, connessione e **pull** di telemetria/allarmi;
  gestione device e attributi. Vedi [thingsboard-manuale](thingsboard-manuale.md).
- **Scenari/casi d'uso**: 16 ambienti attivabili dal superadmin su ogni tenant (moduli + regole
  sensori + demo). Generatore di scenari da descrizione/immagini.

## 11. Assistente AI

L'AI è **attore** nei processi (multi-provider): agente con strumenti che leggono e **scrivono** sul
sistema (le operazioni sensibili richiedono conferma), report persistiti, diario vocale sul campo.
Modulo **assistant**.

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
tenant per assistenza. Le integrazioni (GoAlert/Traccar/ThingsBoard) si **auto-configurano** dai
default di prodotto.

## 14. Riferimenti rapidi (manuali di dettaglio)

| Area | Manuale |
|---|---|
| Console (UI, ricerca, mappe, tema, responsive, i18n) | [console-ui-manuale.md](console-ui-manuale.md) |
| Escalation e reperibilità | [escalation-manuale.md](escalation-manuale.md) · [goalert-manuale.md](goalert-manuale.md) |
| Traccar (badge/GPS) | [traccar-manuale.md](traccar-manuale.md) |
| ThingsBoard (IoT) | [thingsboard-manuale.md](thingsboard-manuale.md) |
| Edge nativo Teltonika (TCP/IO/GPRS) | [teltonika-edge.md](teltonika-edge.md) |
| Gruppi di persone condivisi | [gruppi-condivisi.md](gruppi-condivisi.md) |
| Permessi (capability) | [capability-layer.md](capability-layer.md) |
| Ruoli personalizzati | [custom-roles.md](custom-roles.md) |
| Ronde e punzonature | [ronde-manuale.md](ronde-manuale.md) |

> Questo manuale è l'indice operativo unico; ogni manuale di dettaglio resta la fonte autorevole
> per la sua area e viene aggiornato insieme al codice.
