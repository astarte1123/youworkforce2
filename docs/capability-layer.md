# Permessi (capability) per ruolo

Ogni cosa che si può fare in YouWorkforce è legata a un **permesso** (in codice: *capability*).
Ogni ruolo base ha un insieme di permessi **predefiniti**; un'azienda (tenant) può poi
**aggiungere o togliere** permessi a un ruolo, oppure creare **ruoli personalizzati**
([custom-roles.md](custom-roles.md)), senza toccare il codice.

Questo documento descrive lo stato **attuale** del codice. Fonti: `CAPABILITIES`,
`default_role_caps`, `cap_modules`, `has_cap`, `require_grantable` in `core/src/api.rs`; handler
HTTP in `core/src/iam.rs`; etichette mostrate in console in `CAP_LABELS` (`console/static/app.html`).

Stato delle fasi del progetto permessi:

| Fase | Contenuto | Stato |
|---|---|---|
| 1 | gruppi di persone condivisi ([gruppi-condivisi.md](gruppi-condivisi.md), mig 0072) | **fatta** |
| 2 | permessi per ruolo + modifiche per tenant (questo documento, mig 0073) | **fatta** |
| 3 | ruoli personalizzati ([custom-roles.md](custom-roles.md), mig 0074) | **fatta** |
| 4 | permessi granulari per area e modulo ([iam-refactor-plan.md](iam-refactor-plan.md)), tappe 1-4 | **fatta** |
| — | regole di delega TA-1..TA-4, elenco utenti TA-10, permessi di Campo CA-9..CA-11 | **fatte** (25/11/2026) |
| — | `sites.manage` (gestione sedi e asset, TA-17) | **fatta** (26/11/2026) |

## Catalogo dei permessi

Sono **19** permessi. `admin` è un permesso speciale: chi ce l'ha ha automaticamente tutti gli
altri (`has_cap` risponde sì se l'utente ha il permesso richiesto **oppure** `admin`).

La colonna «Moduli» dice a quali moduli è legato il permesso: nella pagina dei permessi la console
mostra i permessi di base (nessun modulo) e quelli dei moduli attivi del tenant; gli altri sono
nascosti (restano validi, semplicemente non servono).

| Nome in console | Codice | Cosa consente | Moduli | Ruoli che lo hanno di default |
|---|---|---|---|---|
| Amministrazione completa | `admin` | tutto: implica ogni altro permesso | — | tenant_admin |
| Sicurezza · Vedere allarmi | `incidents.view` | leggere Centrale allarmi, mappe, dispositivi, report | loneworker, gps, iot, giri | operatore, responsabile, auditor |
| Sicurezza · Gestire allarmi | `incidents.handle` | prendere in carico, chiudere, annotare, provare gli allarmi | loneworker, gps, iot, giri | operatore, responsabile, reperibile |
| Dispositivi · Inviare comandi | `devices.manage` | inviare comandi ai dispositivi | loneworker, gps, iot | operatore |
| Lavoro sul campo · Pianificare | `plan` | creare e assegnare ordini/interventi, ronde, turni | interventi, cantieri, giri, workforce | pianificatore |
| Consuntivi · Approvare | `approve` | approvare o rimandare i consuntivi | consuntivi | responsabile |
| Consuntivi · Vedere tutti | `timesheets.view_all` | leggere i consuntivi di tutti, non solo i propri | consuntivi | responsabile, auditor |
| Interventi · Vedere tutti | `orders.view_all` | leggere tutti gli ordini delle proprie sedi (CA-10) | interventi | responsabile, auditor |
| Costi · Vedere | `costs.view` | budget, costi, SAL, fatture delle commesse, prezzi dei materiali (CA-11) | cantieri, interventi | responsabile, auditor |
| Configurazione · Sola lettura | `config.view` | leggere configurazione e diagnostica | — | auditor |
| Lavoro personale | `work.own` | vedere nel cruscotto i propri interventi, consuntivi, ronde e turni (IO-16) | interventi, consuntivi, giri, workforce | lavoratore |
| Amministrazione · Utenti e ruoli | `users.manage` | utenti, gruppi di persone, permessi, ruoli personalizzati, sedi, squadre, asset | — | nessuno (solo via `admin`) |
| Amministrazione · Dispositivi | `devices.admin` | censimento dispositivi, chiavi/onboarding, geofence, beacon, piantine, provisioning | loneworker, gps, iot | nessuno (solo via `admin`) |
| IoT · Regole e sensori | `iot.manage` | allarmi dei dispositivi, regole sensori, allarmi da email, monitoraggio IoT | iot, gps | nessuno (solo via `admin`) |
| Escalation e reperibilità | `escalation.manage` | gruppi di escalation, routing, turni di reperibilità, assenze, profili, trigger, recapiti, rete di ultima istanza | loneworker, gps, iot, giri | nessuno (solo via `admin`) |
| Impostazioni del tenant | `settings.manage` | funzioni della console, configurazione guidata, moduli, funzioni AI | — | nessuno (solo via `admin`) |
| Privacy (GDPR) | `gdpr.manage` | export e anonimizzazione dei dati personali | — | nessuno (solo via `admin`) |
| Audit · Registro attività | `audit.view` | leggere il registro attività | — | nessuno (solo via `admin`) |
| Dashboard · Organizzare | `dashboard.manage` | abilitare, ordinare e dimensionare i riquadri della dashboard del tenant | — | nessuno (solo via `admin`) |

> **`sites.manage` (TA-17, decisione PO 26/11/2026, fatto).** Nuovo permesso «gestire le sedi»,
> predefinito per **tenant_admin** (via `admin`) e **pianificatore**. Serve a separare la gestione
> delle sedi da `users.manage`, così il pianificatore può creare e modificare le sedi dei propri
> lavori senza poter gestire persone e permessi. La console lo prevede già sulla voce *Sedi e
> asset*; finché non è nel catalogo del backend (`CAPABILITIES`) non compare nella tabella dei
> permessi e le sedi restano sotto `users.manage`. Aggiornare questa tabella quando entra nel codice.

Nota su `devices.admin` e `devices.manage`: sono distinti. `devices.manage` è l'invio di comandi
(lo ha l'operatore), `devices.admin` è l'amministrazione della flotta (solo amministratore).

Nota sull'auditor: legge allarmi, consuntivi, ordini, costi e configurazione, ma **non** ha
`audit.view` di default: il registro attività lo vede solo l'amministratore, a meno di assegnarlo.

## Matrice predefinita per ruolo

È il comportamento storico: senza modifiche del tenant ogni ruolo può fare esattamente ciò che
poteva fare prima dell'introduzione dei permessi.

| Ruolo | Permessi di default |
|---|---|
| tenant_admin | `admin` (quindi tutti) |
| operatore | `incidents.view`, `incidents.handle`, `devices.manage` |
| pianificatore | `plan`, `sites.manage` |
| responsabile | `approve`, `incidents.view`, `incidents.handle`, `timesheets.view_all`, `orders.view_all`, `costs.view` |
| auditor | `incidents.view`, `timesheets.view_all`, `config.view`, `orders.view_all`, `costs.view` |
| reperibile | `incidents.handle` |
| lavoratore | `work.own` (i widget del proprio lavoro nel cruscotto); ai propri dati accede tramite i controlli «solo i propri» |

I permessi d'area (`users.manage`, `devices.admin`, `iot.manage`, `escalation.manage`,
`settings.manage`, `gdpr.manage`, `audit.view`, `dashboard.manage`) non sono assegnati a nessun
ruolo di default: li ha solo l'amministratore tramite `admin`. Si possono delegare per area con una
modifica della matrice o con un ruolo personalizzato (es. un «DPO» con solo `gdpr.manage` e
`audit.view`).

## Come si calcolano i permessi di un utente

```
permessi(utente) = default del ruolo base ± modifiche del tenant per quel ruolo
                 ∪ permessi dei ruoli personalizzati assegnati all'utente
                 ∪ permessi dei ruoli personalizzati assegnati ai suoi gruppi di persone
```

- Calcolati a **ogni richiesta** (`effective_caps` / `effective_caps_user`): una modifica ha
  effetto subito, senza nuovo accesso.
- **Ruolo ed esistenza dell'utente** si rileggono a ogni richiesta (TA-2): un utente retrocesso ha
  subito i nuovi permessi; eliminato, anonimizzato o di un tenant sospeso perde la sessione (`401`).
- I controlli del backend usano `require_cap(&s, "<permesso>")` o `has_cap`. Un'azione non
  permessa risponde `403` con il permesso mancante.
- `GET /api/me` restituisce `caps` (permessi effettivi) per la console; il backend resta l'autorità.

## Regole di delega (TA-1, TA-3)

**Non si concede ciò che non si possiede** (`require_grantable`). Chi ha `users.manage` senza
`admin` (delegato):
- assegna a utenti, ruoli personalizzati e matrice dei ruoli **solo permessi che ha già**; quindi
  non si promuove, non crea amministratori, non crea né si assegna ruoli con `admin` o con permessi
  che non ha (`403` con l'elenco leggibile dei permessi mancanti);
- non modifica (ruolo, password, telefono) né elimina utenti che hanno permessi che lui non ha, ad
  esempio un amministratore;
- non tocca la riga della matrice di un ruolo che ha più permessi dei suoi; il **ripristino** della
  matrice ai valori predefiniti è riservato all'amministratore (`admin`).

Un amministratore ha tutti i permessi e quindi può creare altri amministratori.

**Mai senza amministratore**: retrocedere o eliminare un `tenant_admin` (sé stessi o un altro) è
rifiutato (`409`) se non resta almeno un altro amministratore attivo. Non si può togliere `admin`
al ruolo `tenant_admin` nella matrice (`400`).

**Permessi, non nomi di ruolo (TA-4).** Controlli che prima richiedevano il ruolo `tenant_admin`
ora richiedono un permesso, delegabile:

| Funzione | Permesso |
|---|---|
| funzioni AI del tenant, configurazione guidata, configurazione degli strumenti di modulo | `settings.manage` |
| beacon, piantine, posizionamento di beacon e dispositivi sulla piantina | `devices.admin` |
| suggerimento AI delle regole sensori | `iot.manage` |

Restano legati al ruolo base, da rivedere con i moduli: la visibilità «solo i propri» di
lavoratore e reperibile (ordini, turni, diario) e alcune voci operative della console (vedi sotto).

e2e: `qa/iam-guardrails-e2e.sh`.

## Elenco utenti (TA-10, 25/11/2026)

`GET /api/users`:
- **telefono** dei colleghi solo a chi ha `users.manage`, `escalation.manage`, `incidents.handle`
  (chi gestisce gli allarmi deve poter chiamare il lavoratore) o `plan` (contattare un tecnico sul
  campo, decisione PO 25/11/2026);
- **elenco con email e ruolo** a chi coordina (`plan`, `incidents.view`, `approve`,
  `timesheets.view_all`, `orders.view_all`, `devices.manage`, `devices.admin`, `iot.manage`,
  `audit.view`, `gdpr.manage`, `settings.manage`, `dashboard.manage`, `config.view`);
- chi non ha nessuna di queste capability vede **solo sé stesso**.

e2e `qa/users-privacy-e2e.sh` 5/5.

## Campo: ordini, costi, posizioni e sedi (CA-9/10/11, 25/11/2026)

- **Ordini (CA-10).** Tutti gli ordini li vede solo chi ha `plan` o `orders.view_all`; gli altri
  vedono i propri e quelli del proprio gruppo di persone. Righe e allegati di un ordine seguono la
  stessa regola (`order_view_allowed`); le indicazioni d'accesso alla sede solo a pianificatore e
  assegnatario. Stessa regola per lo strumento AI «elenco ordini».
- **Costi (CA-11).** Budget, costo orario, costi, importi SAL e fatture delle commesse e i prezzi
  dei materiali degli ordini li vede chi ha `plan` o `costs.view` (`can_see_costs`); agli altri la
  scheda dice «dati economici riservati». Chi non vede i costi **non fissa il prezzo** di una voce:
  vale quello del listino (o 0, da valorizzare in ufficio).
- **Posizioni (CA-11).** L'elenco dispositivi (con le posizioni) e il cruscotto vigilanza sono
  riservati a chi supervisiona (`is_supervisor`: `incidents.view`, `plan`, `devices.manage`,
  `devices.admin` o `iot.manage`); gli altri vedono solo i propri dispositivi.
- **Sedi (CA-9/CA-10).** **Chiunque abbia sedi assegnate** è ristretto a quelle (centrale, ordini,
  consuntivi), tranne chi ha `admin`; nessuna sede assegnata = nessuna restrizione. Un allarme
  **senza sede** resta visibile a tutti gli operatori (SL-6). Approvare o rimandare un consuntivo
  richiede il modulo attivo e, per chi è ristretto, una sede consentita. L'assistente AI legge un
  consuntivo con la stessa regola della UI.
- Prova: `qa/field-permissions-e2e.sh` **22/22**.

## Modifiche per tenant (mig `0073_role_capabilities.sql`)

Tabella `app.role_capabilities(tenant_id, role, capability, granted, updated_at)`, chiave
`(tenant_id, role, capability)`, RLS + GRANT `app_rt`. Tiene **solo** le differenze dal default:
`granted=true` aggiunge, `granted=false` toglie; nessuna riga = default. Se un valore torna uguale
al default la riga viene rimossa.

## API

| Metodo | Rotta | Azione | Permesso richiesto |
|---|---|---|---|
| GET | `/api/capabilities` | catalogo (con moduli e rilevanza) + matrice effettiva ruolo × permesso (flag `default`/`override`) | `users.manage` |
| POST | `/api/capabilities` | `{role, capability, granted}` imposta o azzera una modifica | `users.manage` + regole di delega |
| DELETE | `/api/capabilities` | ripristino: rimuove tutte le modifiche del tenant | `admin` |

Ruolo o permesso sconosciuti → `400`; togliere `admin` a `tenant_admin` → `400`.

## Console

**Configurazione → Persone e permessi** (`#/utenti-ruoli`, permesso `users.manage`): ruoli
personalizzati, utenti, gruppi di persone e la tabella **Permessi** ruolo × permesso con caselle
(una spunta imposta o azzera una modifica), il neretto segnala i valori diversi dal default,
**ripristina default** torna all'origine. La casella `tenant_admin / admin` è bloccata. I permessi
compaiono con nome e spiegazione (`CAP_LABELS`), mai col codice grezzo.

**Menu, pulsanti e widget per permesso (TA-6/FE-2).** Una voce di menu, un pulsante o un widget
compaiono se il ruolo è tra quelli previsti **oppure** l'utente ha il permesso che li apre. I
ruoli predefiniti vedono gli stessi elementi di prima; cambia solo ciò che vede chi riceve un
permesso per delega.

| Elemento | Permesso che lo apre |
|---|---|
| Dispositivi e app, Collegamento dispositivi, modifica di mappe/geofence/piantine | `devices.admin` |
| Regole di sicurezza (Trigger, Profili di protezione), Escalation e reperibilità | `escalation.manage` |
| Monitoraggio IoT, Alert IoT | `iot.manage` |
| Regole sensori | `iot.manage` o `devices.admin` |
| Configurazione guidata, Moduli e AI | `settings.manage` |
| Persone e permessi | `users.manage` |
| Sedi e asset | `sites.manage` (vale anche `users.manage`, per chi lo aveva già) |
| Privacy e audit | `gdpr.manage` o `audit.view` |
| Interventi, Clienti, Listino, Richieste, Contratti, Cantieri, Ronde, Cruscotto vigilanza, Pianificazione (agenda, turni); widget ordini/ronde/turni | `plan` |
| Consuntivi; widget consuntivi | `approve` o `timesheets.view_all` |
| Ore lavorate | `approve`, `timesheets.view_all` o `plan` |
| widget incidenti | `incidents.view` |
| organizzazione della dashboard | `dashboard.manage` |

Restano legati al ruolo base (per non cambiare i ruoli predefiniti): Centrale allarmi e «Assegnati
a me», mappe e report allarmi, piantine, calendario, agente AI, operazioni in blocco, check-in. Il
backend resta comunque l'autorità.

## Isolamento e prove

- **Tenant**: modifiche isolate per RLS; una modifica su un tenant non tocca gli altri.
- **Default identico**: senza modifiche il comportamento coincide con quello storico.
- Prove: `qa/capabilities-e2e.sh` (default, modifiche con effetto immediato, isolamento,
  anti-lockout, validazione, ripristino), `qa/capabilities-granular-e2e.sh`,
  `qa/capabilities-admin-split-e2e.sh`, `qa/iam-guardrails-e2e.sh`, `qa/users-privacy-e2e.sh`,
  `qa/field-permissions-e2e.sh`, `qa/custom-roles-e2e.sh`.

## Cruscotto: widget per permesso (IO-16, 26/11/2026)

`dashboard::DASHBOARD_WIDGETS` non usa più i nomi dei ruoli: un widget è visibile se uno dei suoi
moduli è attivo e la persona ha uno dei suoi permessi. Così vale anche per i ruoli personalizzati.

| Widget | Moduli | Permessi (uno basta) |
|---|---|---|
| Incidenti | loneworker, gps, iot, giri | `incidents.view` |
| Dispositivi IoT (nuovo) | iot | `iot.manage`, `devices.manage`, `config.view` |
| Check-in attivi | loneworker | `incidents.view` |
| Ordini di lavoro | interventi | `plan`, `orders.view_all`, `work.own` |
| Consuntivi | consuntivi | `approve`, `timesheets.view_all`, `work.own` |
| Ronde | giri | `plan`, `config.view`, `work.own` |
| Turni | workforce | `plan`, `config.view`, `work.own`, `incidents.handle` |
| Il tuo ruolo | — | — |

Con i permessi predefiniti i ruoli vedono gli stessi widget di prima, con tre aggiunte: il
responsabile vede check-in e ordini delle sue sedi, operatore e responsabile i turni, chi gestisce i
dispositivi (operatore, auditor, amministratore) il widget IoT. e2e `qa/dashboard-caps-e2e.sh`.
