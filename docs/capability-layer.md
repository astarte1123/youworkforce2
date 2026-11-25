# Capability layer — Fase 2

Le autorizzazioni diventano **capability** mappate di **default sui 7 ruoli fissi** (comportamento
**identico** a prima). Un tenant può poi **aggiungere o togliere** capability a un ruolo, senza
toccare il codice. Additivo e non distruttivo; i ruoli restano, le capability li raffinano.

## Catalogo capability (Fase 4 Tappa 1 — granulare, module-neutral)

`admin` è **super**: implica tutte le altre (`has_cap` ritorna true se l'utente ha la cap richiesta
**oppure** `admin`).

| Capability | Significato | Controlli coinvolti |
|---|---|---|
| `admin`              | amministrazione completa (super): implica tutte | — (resta assegnata a `tenant_admin`) |
| `incidents.view`     | lettura centrale/mappe/dispositivi/report | ~11 endpoint (prima `operatore/auditor/responsabile` cablati) |
| `incidents.handle`   | gestione allarmi (ack/chiusura/note/test) | ack/close/assign/note/alert-action/ack-all |
| `devices.manage`     | comandi/controllo dispositivi | device_command, RPC, scenario set |
| `plan`               | pianificazione (ordini, ronde, turni) | `is_planner` |
| `approve`            | approvazione consuntivi | `is_approver` |
| `timesheets.view_all`| lettura di tutti i consuntivi | list/get/export/summary consuntivi |
| `orders.view_all`    | lettura di tutti gli ordini (interventi) delle proprie sedi | list_orders, righe/allegati ordine, assistente (CA-10) |
| `costs.view`         | budget, costi, SAL, fatture commesse; prezzi materiali | get/list commesse, righe ordine (CA-11; anche con `plan`) |
| `config.view`        | lettura configurazione/diagnostica | module-tools, assistant plan |
| `users.manage`       | utenti/gruppi/permessi/ruoli custom/sedi/squadre/asset | ~32 endpoint (Tappa 2) |
| `devices.admin`      | censimento/token/onboarding/geofence/provisioning | ~20 endpoint (Tappa 2) |
| `iot.manage`         | dispositivi, regole sensori, allarmi da email | ~31 endpoint (Tappa 2) |
| `escalation.manage`  | gruppi allarme/on-call/override/profili/trigger/recapiti | ~60 endpoint (Tappa 2) |
| `settings.manage`    | impostazioni tenant (UI/feature) | ui_features_set (Tappa 2) |
| `gdpr.manage`        | export/anonimizzazione dati personali | gdpr_export/anonymize/retention |
| `audit.view`         | lettura registro attività | audit_log_list |

> Le 7 capability sotto `config.view` sono le **aree amministrative** (Tappa 2): split dei 149
> endpoint prima `admin_only`. **Nessun ruolo le ha di default** (solo `tenant_admin` via `admin`
> super) ⇒ comportamento invariato; diventano delegabili a ruoli custom/override per area.
> `devices.admin` (amministrazione flotta) è **distinta** da `devices.manage` (invio comandi, che
> l'operatore ha di default). Prova: `qa/capabilities-admin-split-e2e.sh`.

## Mappa di default (= comportamento storico, `admin`=super)

| Ruolo | capability di default |
|---|---|
| tenant_admin | **admin** (⇒ tutte) |
| operatore | incidents.view, incidents.handle, devices.manage |
| pianificatore | plan |
| responsabile | approve, incidents.view, incidents.handle, timesheets.view_all, orders.view_all, costs.view |
| auditor | incidents.view, timesheets.view_all, config.view, orders.view_all, costs.view |
| reperibile | incidents.handle |
| lavoratore | — (accesso ai propri dati via controlli "owner", non capability) |

> I ~36 controlli prima cablati su stringhe-ruolo ora passano da `require_cap`/`has_cap`: i **ruoli
> custom** e gli **override** per-tenant li controllano davvero, su ogni modulo. Restano role-based i
> soli controlli di **scope sui propri dati** (owner/sede). Gli endpoint prima `admin_only` sono
> stati spezzati nelle 7 capability d'area (Tappa 2 FATTA).
> Prova granularità: `qa/capabilities-granular-e2e.sh`. Piano completo: [iam-refactor-plan.md](iam-refactor-plan.md).

### Elenco utenti (TA-10, 25/11/2026)

`GET /api/users` restituiva a chiunque email, ruolo e telefono di tutti. Ora:
- **telefono** solo a chi ha `users.manage`, `escalation.manage` o `incidents.handle` (chi
  gestisce gli allarmi deve poter chiamare il lavoratore);
- **elenco con email e ruolo** a chi coordina (`plan`, `incidents.view`, `approve`,
  `timesheets.view_all`, `orders.view_all`, `devices.*`, `iot.manage`, `audit.view`,
  `gdpr.manage`, `settings.manage`, `dashboard.manage`, `config.view`);
- chi non ha nessuna di queste capability vede **solo sé stesso**.
- e2e `qa/users-privacy-e2e.sh` 4/4.

### Campo: ordini, costi, posizioni e sedi (CA-9/10/11, 25/11/2026)

- **Ordini (CA-10).** Prima chiunque non avesse il ruolo base «lavoratore» (operatore, reperibile,
  ruoli personalizzati…) vedeva tutti gli ordini. Ora tutti gli ordini li vede solo chi ha `plan` o
  `orders.view_all`; gli altri vedono i propri e quelli del proprio gruppo di persone. Righe e
  allegati di un ordine seguono la stessa regola (`order_view_allowed`); le indicazioni d'accesso
  alla sede solo a pianificatore e assegnatario. Stessa regola per lo strumento AI «elenco ordini».
- **Costi (CA-11).** Budget, costo orario, costi, importi SAL e fatture delle commesse e i prezzi
  dei materiali degli ordini li vede chi ha `plan` o `costs.view` (`can_see_costs`); agli altri la
  scheda dice «dati economici riservati». Chi non vede i costi **non fissa il prezzo** di una voce:
  vale quello del listino (o 0, da valorizzare in ufficio).
- **Posizioni (CA-11).** L'elenco dispositivi (con le posizioni) e il cruscotto vigilanza sono
  riservati a chi supervisiona (`is_supervisor`: `incidents.view`, `plan`, `devices.manage`,
  `devices.admin` o `iot.manage`); gli altri vedono solo i propri dispositivi.
- **Sedi (CA-9/CA-10).** `allowed_sites` prima restringeva solo i ruoli base operatore/lavoratore/
  reperibile: un responsabile con sedi assegnate approvava ovunque. Ora **chiunque abbia sedi
  assegnate** è ristretto a quelle (centrale, ordini, consuntivi), tranne chi ha `admin`. Approvare
  o rimandare un consuntivo richiede il modulo attivo e, per chi è ristretto, una sede consentita
  (un consuntivo senza sede non è approvabile da un ruolo ristretto). L'assistente AI legge un
  consuntivo con la stessa regola della UI.
- Prova: `qa/field-permissions-e2e.sh` **22/22**.

## Come funziona

- La **sessione** porta le capability effettive (`caps`), calcolate a ogni richiesta in
  `effective_caps(tenant, role)` = `default_role_caps(role)` ± override del tenant. Quindi un
  override ha **effetto immediato** sulle sessioni già aperte (nessun re-login).
- I controlli consultano le caps: `admin_only` → `admin`, `is_planner` → `plan`,
  `is_approver` → `approve`. Avendo cambiato solo il corpo degli helper, **tutti** gli endpoint
  esistenti onorano gli override senza modificarne i call site.
- `GET /api/me` espone `caps` per usi lato UI (facoltativo; il backend resta l'autorità).
- **Ruolo ed esistenza dell'utente** si rileggono a ogni richiesta (TA-2): un utente retrocesso ha
  subito i nuovi permessi; eliminato, anonimizzato o di un tenant sospeso perde la sessione (`401`).

## Regole di delega (TA-1, TA-3)

**Non si concede ciò che non si possiede.** Chi ha `users.manage` senza `admin` (delegato):
- assegna a utenti, ruoli personalizzati e matrice dei ruoli **solo permessi che ha già**; quindi non
  si promuove, non crea amministratori, non crea né si assegna ruoli con `admin` o con permessi che
  non ha (`403` con l'elenco leggibile dei permessi mancanti);
- non modifica (ruolo, password, telefono) né elimina utenti che hanno permessi che lui non ha, ad
  esempio un amministratore;
- non tocca la riga della matrice di un ruolo che ha più permessi dei suoi; il **ripristino** della
  matrice ai valori predefiniti è riservato all'amministratore.

Un amministratore (`admin`) ha tutti i permessi e quindi può creare altri amministratori.

**Mai senza amministratore**: retrocedere o eliminare un `tenant_admin` (sé stessi o un altro) è
rifiutato (`409`) se non resta almeno un altro amministratore attivo.

**Permessi, non nomi di ruolo (TA-4).** I controlli che prima richiedevano il ruolo `tenant_admin`
ora richiedono un permesso, delegabile con un ruolo personalizzato:

| Funzione | Permesso |
|---|---|
| funzioni AI del tenant, configurazione guidata, configurazione degli strumenti di modulo | `settings.manage` |
| beacon, piantine, posizionamento di beacon e dispositivi sulla piantina | `devices.admin` |
| suggerimento AI delle regole sensori | `iot.manage` |

Con la matrice predefinita nulla cambia: questi permessi li ha solo l'amministratore. Restano legati
al ruolo, da rivedere con i moduli, la visibilità «solo i propri» di lavoratore e reperibile
(ordini, turni, diario) e la sede ammessa per i ruoli operativi.

e2e: `qa/iam-guardrails-e2e.sh`.

## Override per-tenant (mig `0073_role_capabilities.sql`)

Tabella `app.role_capabilities(tenant_id, role, capability, granted, updated_at)` PK
`(tenant_id,role,capability)`, RLS + GRANT app_rt. Tiene **solo** le differenze dal default:
`granted=true` aggiunge, `granted=false` toglie; nessuna riga = default. Se un set coincide col
default, l'override viene rimosso (niente righe inutili).

## API (tenant_admin)

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/capabilities` | catalogo + matrice effettiva ruolo×capability (con flag `default`/`override`) |
| POST | `/api/capabilities` | `{role, capability, granted}` imposta/azzera un override |
| DELETE | `/api/capabilities` | reset: rimuove tutti gli override del tenant |

Guardie: ruolo/capability sconosciuti → 400; **anti-lockout**: non si può togliere `admin` a
`tenant_admin` → 400.

## UI

**Utenti e sedi → «Permessi (capability)»**: matrice ruolo×capability con checkbox (toggle =
set/azzera override), un pallino segnala i valori modificati rispetto al default, pulsante
«ripristina default». La casella `tenant_admin/admin` è bloccata (anti-lockout).

**Menu, pulsanti e widget per permesso (TA-6/FE-2).** La console legge i permessi effettivi da
`GET /api/me` (`caps`). Una voce di menu, un pulsante o un widget della dashboard compaiono se il
ruolo è tra quelli previsti **oppure** l'utente ha il permesso che li apre (ad esempio con un ruolo
personalizzato). I permessi sono scelti in modo che **i ruoli predefiniti vedano esattamente gli
stessi elementi di prima**; cambia solo ciò che vede chi riceve un permesso per delega.

| Elemento | Permesso che lo apre |
|---|---|
| Dispositivi & App, Collegamento dispositivi, modifica di mappe/geofence/piantine | `devices.admin` |
| Trigger di sicurezza, Profili di protezione, Escalation e reperibilità | `escalation.manage` |
| Configurazione, AI, Moduli | `settings.manage` |
| Utenti e ruoli, Utenti e sedi | `users.manage` |
| Privacy e audit | `gdpr.manage` o `audit.view` |
| Monitoraggio IoT, Alert IoT | `iot.manage` |
| pagine e pulsanti di pianificazione (interventi, clienti, listino, agenda, cantieri, ronde, turni…); widget ordini/ronde/turni | `plan` |
| Consuntivi, approvazione; widget consuntivi | `approve` o `timesheets.view_all` |
| widget incidenti | `incidents.view` |

Restano legati al ruolo (per non cambiare i ruoli predefiniti): Centrale, mappe e report allarmi,
presa in carico e operazioni in blocco, check-in. Il backend resta comunque l'autorità: un'azione non
permessa risponde `403` con il permesso mancante in chiaro.

## Isolamento e compatibilità

- **Tenant**: override isolati per RLS; un override su un tenant non tocca gli altri.
- **Default identico**: senza override il comportamento è byte-identico; suite di regressione
  verdi (loneworker 15, escalation-decouple 13, field-slice1 8, user-groups 16).
- **Prove**: `qa/capabilities-e2e.sh` **17/17** (default identico, override add/remove con effetto
  immediato, isolamento tenant, anti-lockout, validazione, reset).

## Rapporto con le altre fasi

- **Fase 1** [gruppi condivisi](gruppi-condivisi.md): insiemi di persone riusabili.
- **Fase 3 (progettata, non fatta)**: ruoli personalizzati per tenant = insiemi di capability
  nominati dal tenant, assegnabili a utenti e/o ai gruppi di persone della Fase 1.
