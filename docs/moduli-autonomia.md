# Autonomia e coesistenza dei moduli

Ogni tenant riceve dal superadmin un sottoinsieme di moduli (`tenant_modules.state='attivo'`). I due
perimetri di prodotto devono funzionare **da soli** e **insieme**:

- **Sicurezza lavoratore** — `loneworker` (centrale, check-in, SOS, escalation) + connettori
  `goalert`/`traccar`/`thingsboard`/`iot`.
- **Lavoro sul campo** — `interventi` (ordini), `giri` (ronde), `consuntivi`, `workforce` (turni).

## Principio di gating

- Backend: ogni endpoint è protetto dal proprio modulo via `require_module` / `module_active` /
  `require_any_module` (api.rs). Il controllo è **per-richiesta** → l'attivazione/disattivazione ha
  effetto immediato, senza re-login.
- Console: le voci di menu sono filtrate per ruolo **e** per modulo (`modOk` in app.html); i gruppi
  di sezione vuoti non vengono resi. La **Sicurezza lavoratore** appare solo con un modulo safety
  attivo; il **Lavoro sul campo** solo con un modulo Campo attivo.
- Primitive **module-neutral** (condivise, mai gated da un modulo): utenti, sedi, gruppi di persone,
  capability, ruoli custom, privacy/audit, asset. Le **squadre** invece appartengono a `workforce`.

## Perché check-in/SOS sono legati a loneworker

Check-in e SOS creano `incidents` + `outbox`. La loro visibilità/gestione (Centrale, `list_incidents`)
è gated dai moduli che producono allarmi (`ESCALATION_MODS = loneworker,traccar,iot,thingsboard`). Se
si mostrasse il check-in/SOS senza loneworker attivo, l'allarme generato non sarebbe visibile a
nessuno (black-hole = falso senso di sicurezza). Perciò:

- `create_checkin`/`confirm_checkin` richiedono `loneworker` (api.rs);
- la PWA worker mostra i riquadri SOS / check-in / condivisione posizione **solo** se `loneworker` è
  attivo (`safetyWorker()` in console/static/worker/index.html);
- l'SOS resta fail-open quando il perimetro safety è attivo.

## Diario di lavoro

Il diario (`/api/diary`) appartiene al Campo: richiede `workforce` **o** `interventi`
(`require_any_module`). La PWA mostra "Detta una nota" solo se uno dei due è attivo (`fieldDiary()`).
Lo stesso vale per il tool AI `add_diary_entry`.

## Dashboard console

I widget della dashboard (`vDashboard`) dipendono da ruolo **e** modulo: un modulo spento non viene
interrogato e non mostra né KPI né card (prima un 403 veniva mascherato in lista vuota, mostrando es.
"Incidenti aperti: 0" su un tenant di solo Campo).

## FK dormiente (nota)

`patrol_checkpoints.geofence_id → app.geofences` (mig 0075): `geofences` è una tabella traccar, ma la
colonna è nullable, `ON DELETE SET NULL` e **mai letta/scritta** dagli handler `giri`. Nessun impatto
sull'autonomia di `giri`; lasciata com'è (rimuoverla richiederebbe una migrazione non necessaria).

## Prove

`qa/module-autonomy-e2e.sh` — **18/18**. Tenant throwaway, tre scenari su un unico insieme
utenti (gating per-richiesta, nessun re-login), teardown finale (non tocca globex):

- **A · solo Sicurezza** (loneworker): check-in 200, incidenti 200; ordini/consuntivi/squadre 403;
  diario 403.
- **B · solo Campo** (interventi/consuntivi/workforce/giri): check-in 403, incidenti 403; ordini/
  consuntivi/squadre 200; diario 200.
- **C · entrambi**: tutto 200 (coesistenza).

In CORE di `qa/run-all.sh`.
