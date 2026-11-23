# Dashboard unica configurabile

Una sola dashboard, composta da **widget** che fanno riferimento ai **moduli attivi** del tenant.
Il **tenant_admin** la organizza (abilita/nasconde, riordina, dimensiona); l'attivazione dei
**moduli** resta del **superadmin** (tenant_modules). La dashboard è il primo consumatore
*trasversale*: aggrega funzioni di qualunque modulo attivo in modo uniforme.

## Catalogo widget (single source of truth)

In codice: `DASHBOARD_WIDGETS` (api.rs) — `(key, modules-any-of, roles-any-of, size-default)`.
Un widget è candidato se **un suo modulo è attivo** (lista vuota = sempre, es. core) **e** il ruolo
dell'utente è tra quelli ammessi (lista vuota = tutti). Widget iniziali:

| key | moduli | note |
|-----|--------|------|
| `incidents` | loneworker/gps/iot | aperti + ultimi |
| `checkins` | loneworker | check-in attivi |
| `orders` | interventi | ordini attivi + ultimi |
| `timesheets` | consuntivi | in revisione |
| `rounds` | giri | giri attivi |
| `shifts` | workforce | turni pianificati |
| `role` | — (core) | ruolo utente, sempre |

Il render di ogni widget è lato console (`DASH_WIDGETS` in app.html): ogni voce dichiara solo
`title`, `load()` e `body(data)`. Aggiungere un widget = una riga nel catalogo backend + una nel
registro console.

## Layout per-tenant

Tabella `app.dashboard_layouts(tenant_id, widget_key, enabled, position, size)` (mig 0077, RLS,
additiva). **Assenza di righe = default**: tutti i widget dei moduli attivi, ordine del catalogo,
`size=half` (= comportamento storico, non distruttivo).

- `GET /api/dashboard/layout` (ogni utente): catalogo filtrato per moduli attivi + ruolo, merge col
  layout salvato, ordinato per `position`. Risposta `{widgets:[{key,enabled,position,size}], can_edit}`.
  `can_edit` = capability `dashboard.manage`.
- `PUT /api/dashboard/layout` (capability **`dashboard.manage`**, di default solo `tenant_admin`):
  body `{widgets:[{widget_key,enabled,position,size}]}`. Upsert; chiavi fuori catalogo ignorate;
  `size` validata (`half|full`). Tracciato in audit (`dashboard_layout_set`).

**Garanzia:** un widget il cui modulo è spento **non viene mai esposto**, anche se presente nel
layout salvato (il filtro moduli è applicato a ogni GET). Così attivare/disattivare un modulo
(superadmin) resta la fonte di verità su cosa esiste; il layout (tenant_admin) decide solo
disposizione e visibilità tra ciò che esiste.

## UI console

Dashboard data-driven (`vDashboard`): carica il layout, poi ogni widget abilitato in parallelo.
Il `tenant_admin` ha il pulsante **⚙ Organizza**: trascina (⠿) per riordinare, attiva/nascondi,
alterna dimensione metà/intera, poi **Salva layout** (PUT). Gli altri ruoli vedono la dashboard in
sola lettura, già filtrata per i loro permessi.

## Separazione dei poteri

- **Moduli** on/off → **solo superadmin** (`require_super`, tenant_modules).
- **Widget/layout** → **tenant_admin** (`dashboard.manage`).

## Prove

`qa/dashboard-layout-e2e.sh` — **10/10**: GET filtrato per moduli+ruolo e `can_edit`; PUT solo admin
(pianificatore 403); layout salvato rispettato (enabled/position/size); widget di modulo spento mai
esposto anche se nel layout; chiave ignota ignorata; ordinamento per position; traccia audit. In CORE
di `qa/run-all.sh`.
