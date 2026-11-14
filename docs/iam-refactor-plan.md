# Piano — Permessi granulari module-neutral (IAM Fase 4)

Obiettivo: rendere **utenti/gruppi/ruoli** davvero flessibili e non sbilanciati sulla sicurezza.
Oggi la flessibilità (capability + ruoli custom) controlla **solo 3 cose** (`admin/plan/approve`) e
**36 controlli** sono cablati su stringhe-ruolo (operatore/auditor/reperibile/…), che i ruoli custom
**non** toccano. Questo piano introduce un **catalogo capability per-modulo** e migra tutti i
controlli a `require_cap`, **mantenendo il comportamento di default identico** (default-preserving).

## Principi

- **Additivo e non distruttivo**: la mappa `default_role_caps` riproduce ESATTAMENTE chi passa oggi.
- **`admin` resta super**: implica tutte le capability (nessuna regressione per il tenant_admin).
- Ogni capability nasce dal **set-ruoli storico** del punto che sostituisce (così nessuno guadagna o
  perde accesso al cutover).
- I **ruoli custom** potranno comporre QUALSIASI capability → granularità vera per ogni modulo.

## 1. Catalogo capability proposto (~20, per area)

| Area | Capability | Significato | Set-ruolo storico (default) |
|---|---|---|---|
| IAM | `users.manage` | utenti/gruppi/ruoli | tenant_admin |
| IAM | `settings.manage` | impostazioni tenant, moduli-tool, branding | tenant_admin |
| IAM | `audit.view` | log di audit (nuovo viewer) | tenant_admin, auditor |
| IAM | `gdpr.manage` | export/anonimizzazione (nuovo pannello) | tenant_admin |
| Sicurezza | `incidents.view` | Centrale/mappa/dettaglio incidenti (lettura) | operatore, tenant_admin, auditor |
| Sicurezza | `incidents.handle` | ack/close/note/test-SOS | operatore, tenant_admin |
| Sicurezza | `oncall.handle` | reperibile: vista/ack propri allarmi | operatore, tenant_admin, reperibile |
| Sicurezza | `escalation.manage` | gruppi allarme, routing, turni, recapiti | tenant_admin |
| Sicurezza | `safety.config` | trigger, profili di protezione | tenant_admin |
| Dispositivi | `devices.view` | elenco/stato/posizioni dispositivi | operatore, tenant_admin, auditor |
| Dispositivi | `devices.manage` | crea/assegna/comandi/onboarding/GPRS | operatore, tenant_admin |
| IoT | `iot.manage` | connettori, regole sensori, pull | tenant_admin |
| Campo·interventi | `orders.manage` | crea/assegna/pianifica/annulla/chiudi | pianificatore, tenant_admin |
| Campo·interventi | `orders.execute` | esegui il proprio ordine (arrivo/report) | lavoratore (assegnatario) |
| Campo·interventi | `orders.review` | revisione report/consuntivo ordine | pianificatore, responsabile, tenant_admin |
| Campo·ronde | `patrols.manage` | configura giri/checkpoint | pianificatore, tenant_admin |
| Campo·ronde | `patrols.execute` | esegui ronda/punzona | lavoratore, reperibile, tenant_admin |
| Consuntivi | `timesheets.submit` | invia i propri consuntivi | lavoratore |
| Consuntivi | `timesheets.approve` | approva/rifiuta | responsabile, tenant_admin |
| Workforce | `shifts.manage` | pianifica turni | pianificatore, tenant_admin |
| Workforce | `shifts.view_own` | vede/conferma i propri turni | lavoratore, reperibile |
| Report | `reports.view` | report allarmi/attività | operatore, responsabile, tenant_admin, auditor |

> I set-ruolo sono quelli **attuali** (misurati). Dove un endpoint usa un set diverso da un altro
> con stessa semantica, si tengono capability separate o si decide (vedi §4 Decisioni).

## 2. I 36 punti da migrare (api.rs) → capability

Raggruppati per capability (riga : controllo attuale → cap):

- **incidents.view** (`operatore|tenant_admin|auditor`): 579, 2285, 4059, 5071, 5272, 6094 (+ 625,
  5296, 5619, 5680 che aggiungono `responsabile` → valutare `incidents.view` esteso o `reports.view`).
- **incidents.handle** (`operatore|tenant_admin`): 599, 2007, 4024, 4042, 5025, 5090, 8095.
- **oncall.handle** (`operatore|tenant_admin|reperibile`): 6289, 6310; (3835 aggiunge `responsabile`).
- **audit.view / config read** (`tenant_admin|auditor`): 7062, 7080(editable), 7749.
- **orders.manage / review** (`is_planner`, `is_approver`, `s.role=="lavoratore"`, `auditor`):
  2653, 7164, 7193, 7230, 7261, 7291, 7303, 7318, 7331, 7421, 7445, 7506, 7571, 7588, 7604, 7903, 7931.
- **patrols.manage / execute** (`is_planner`, own): 4269–4350, 4406, 4424, 4435, 4475, 4566.
- **shifts.manage** (`is_planner`): 4618, 4629, 4658, 4669; `shifts.view_own` 4589.
- **self/field read** (`operatore|lavoratore|reperibile`): 2830.
- **reports.view** (`operatore|responsabile|tenant_admin|auditor`): 6578 (`all`), 5296, 5619, 5680.

(Elenco operativo: ogni riga diventa `require_cap(&s,"<cap>")?;` o `has_cap(&s,"<cap>")`.)

## 3. Implementazione (default-preserving)

1. `CAPABILITIES`: sostituire i 3 valori con il catalogo §1 (codice + descrizione + `area`).
2. `default_role_caps(role)`: mappa esplicita ruolo→Vec<cap> = §1 (admin ⇒ tutte).
3. `fn has_cap` resta; aggiungere `fn require_cap(&s,cap) -> Result<(),(StatusCode,String)>` e, per
   `admin`, far sì che `has_cap` ritorni true se l'utente ha `admin` **o** la cap richiesta
   (admin = super).
4. Riscrivere `admin_only`→`require_cap("settings.manage"/"users.manage")` a seconda del punto (oggi
   è un unico `admin`: va **splittato** per punto secondo §1), `is_planner`→`orders.manage`/
   `patrols.manage`/`shifts.manage` secondo contesto, `is_approver`→`timesheets.approve`.
5. Migrare i 36 `matches!(s.role…)` ai `require_cap`.
6. `effective_caps`/`effective_caps_user`: invariati (già uniscono default + override + ruoli custom);
   funzioneranno automaticamente sul nuovo catalogo.
7. UI matrice Permessi + Ruoli custom: nessuna modifica strutturale (leggono `catalog`); solo più
   colonne. Opz.: raggruppare le capability per `area` nella matrice.

## 4. Decisioni aperte (da confermare)

- **Granularità `admin`**: splittarlo in `users.manage`+`settings.manage`+`escalation.manage`+
  `safety.config`+`iot.manage` (consigliato) oppure tenere un `admin` unico che le implica? (Il
  default tenant_admin ha comunque tutto.)
- **auditor**: definirlo come insieme di sole `*.view` (read-only trasversale) — confermare.
- **set-ruolo non uniformi** (es. +responsabile su alcune letture incidenti): uniformare a
  `incidents.view` (piccolo allargamento, 1-2 endpoint) o tenere `reports.view` separato?
- **Ruolo custom come PRIMARIO** dell'utente (oltre che additivo): in scope o no?

## 5. Gruppi come consumatori di altri moduli (opzione B)

Oggi i gruppi condivisi servono solo l'import nei gruppi allarme. Estensione proposta:

- **Assegnazione a gruppo**: `work_orders.assigned_group_id` / `patrol_routes` assegnabili a un
  gruppo; i membri vedono/eseguono. (mig additiva, colonne nullable.)
- **ACL per gruppo**: visibilità sedi/ordini per appartenenza a gruppo (oggi l'ACL sedi è per-utente).

## 6. Test

- e2e nuovo `qa/capabilities-granular-e2e.sh`: per ogni capability, un ruolo che la **ha** passa e uno
  che **non** la ha riceve 403; override e ruolo custom che concede la cap sbloccano l'endimodulo.
- **Regressione**: l'intera suite `MODE=core` deve restare verde (default-preserving) — è la prova
  che il cutover non cambia comportamento.

## 7. Rischi

- Refactor ampio (36 punti + helper). Mitigazione: default-preserving + suite e2e esistente come rete.
- `admin_only` oggi è un unico cap: lo split richiede di decidere per ciascuno dei ~130 call-site a
  quale `*.manage` appartiene. Si può fare a tappe (prima tutto → `settings.manage`, poi affinare).

## Stima a tappe

1. **Tappa 1 — FATTA** (catalogo granulare + `require_cap`/`has_cap` super-admin + migrazione di ~27
   `matches!(s.role)` e dei read-consuntivi a capability). Catalogo a 8 cap (admin super +
   incidents.view/handle, devices.manage, plan, approve, timesheets.view_all, config.view). Default
   = comportamento storico (regressione `MODE=core` verde 37/39, i 2 noti ambientali). Nuova
   granularità provata da `qa/capabilities-granular-e2e.sh` 7/7. `admin_only` (~130) e i controlli di
   scope owner/sede restano invariati.
2. **Tappa 2**: split di `admin` nei `*.manage` per-area sui ~130 call-site.
3. **Tappa 3 (opz. B)**: gruppi consumatori in Campo + ACL per gruppo.
4. **Tappa 4 (compliance)**: pannello GDPR + viewer audit log (capability `gdpr.manage`/`audit.view`).
