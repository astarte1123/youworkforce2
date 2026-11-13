# Capability layer — Fase 2

Le autorizzazioni diventano **capability** mappate di **default sui 7 ruoli fissi** (comportamento
**identico** a prima). Un tenant può poi **aggiungere o togliere** capability a un ruolo, senza
toccare il codice. Additivo e non distruttivo; i ruoli restano, le capability li raffinano.

## Catalogo capability

| Capability | Significato | Controlli coinvolti |
|---|---|---|
| `admin`   | amministrazione (impostazioni, utenti, dispositivi, gruppi, config escalation) | `admin_only` (≈130 endpoint) |
| `plan`    | pianificazione (ordini/interventi, turni) | `is_planner` |
| `approve` | approvazione consuntivi | `is_approver` |

## Mappa di default (= comportamento storico)

| Ruolo | capability di default |
|---|---|
| tenant_admin | admin, plan, approve |
| pianificatore | plan |
| responsabile | approve |
| operatore / lavoratore / reperibile / auditor | — |

## Come funziona

- La **sessione** porta le capability effettive (`caps`), calcolate a ogni richiesta in
  `effective_caps(tenant, role)` = `default_role_caps(role)` ± override del tenant. Quindi un
  override ha **effetto immediato** sulle sessioni già aperte (nessun re-login).
- I controlli consultano le caps: `admin_only` → `admin`, `is_planner` → `plan`,
  `is_approver` → `approve`. Avendo cambiato solo il corpo degli helper, **tutti** gli endpoint
  esistenti onorano gli override senza modificarne i call site.
- `GET /api/me` espone `caps` per usi lato UI (facoltativo; il backend resta l'autorità).

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
