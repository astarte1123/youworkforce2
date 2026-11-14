# Privacy e audit (GDPR + registro attività)

Voce menu **«Privacy e audit»** (console → gruppo *Configurazione*, solo `tenant_admin`).
Riunisce tre strumenti di compliance per il tenant: diritto d'accesso (export), diritto
all'oblio (anonimizzazione), policy di ritenzione e il registro delle attività (audit log).

## GDPR · dati personali

Tabella di tutti gli utenti del tenant con stato (attivo / anonimizzato) e due azioni.

### Export (diritto d'accesso)
`GET /api/gdpr/export/:uid` → JSON con i dati personali della persona nel tenant:
profilo (email, ruolo, telefono, date, `anonymized_at`), check-in, diario, reperibilità
(oncall) e incidenti assegnati (max 1000 per sezione). La console scarica il file
`gdpr-<email>.json` (blob lato browser). Ogni export è tracciato in audit (`gdpr_export`).

### Anonimizza (diritto all'oblio)
`POST /api/gdpr/anonymize/:uid`. Non elimina la riga (preserva l'integrità storica degli
incidenti): scrubba la PII e blocca il login. In pratica:
- email → `anon-<8hex>@anonymized.invalid`, telefono → NULL, `goalert_user_id` → NULL,
  password → token casuale (login impossibile), `anonymized_at` = ora;
- corpo delle voci di diario → `[rimosso: anonimizzato]`;
- sessioni attive dell'utente cancellate.

Protezioni: non puoi anonimizzare **te stesso** (400), né **l'ultimo** `tenant_admin`
attivo (400); se l'utente ha **incidenti aperti** assegnati → 409 (riassegnali/chiudili
prima). Tracciato in audit (`gdpr_anonymize`).

## Ritenzione dati

`GET /api/gdpr/retention` → `{enabled, days, note}` (sola lettura). La policy è configurata
dall'amministratore di sistema via env **`YW_RETENTION_DAYS`** (0 = disattivata, default).
Quando attiva, il worker `purge_retention` elimina in modo conservativo, oltre N giorni:
incidenti **chiusi** (+ relativa timeline), eventi non referenziati e lo stesso audit log.
La UI mostra solo lo stato (non modifica la policy: è una scelta di deployment).

## Registro attività (audit log)

`GET /api/audit-log` (tenant-scoped, `admin_only`). Parametri query opzionali:
- `action` — filtro esatto sull'azione;
- `actor` — sottostringa (ILIKE) sull'attore (es. email);
- `limit` — 1..1000, default 200.

Risposta: `{rows:[{id,at,actor,action,detail}], actions:[...], count}` ordinata per data
discendente. `actions` è l'elenco distinto per popolare il filtro a tendina. La tabella
`app.audit_log` (colonne `tenant_id, actor, action, detail jsonb, at`) è scritta in tutta
l'applicazione (login/impersonate, set modulo/connettore, config tool, operazioni GDPR,
ecc.); questa è la **prima** interfaccia di lettura.

## Permessi

Tutti gli endpoint sono `admin_only` (capability `admin`). Le capability dedicate
`gdpr.manage` / `audit.view` sono rimandate alla **Tappa 2** dello split di `admin`
(vedi `iam-refactor-plan.md`).

## Prove

`qa/gdpr-audit-e2e.sh` — **12/12**: audit-log admin 200 (rows+actions) e non-admin 403;
retention admin 200 / non-admin 403; export profilo e non-admin 403; anonymize self→400,
esecuzione→200, PII rimossa, login bloccato; audit traccia filtrabile per azione e attore.
In CORE di `qa/run-all.sh`.
