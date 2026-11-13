# Gruppi di persone condivisi — Fase 1

Primitiva comune **additiva e non distruttiva**: «un insieme di persone» senza comportamento,
riusabile da qualunque modulo. Owner canonico = **core**. Non sostituisce i tre raggruppamenti
esistenti, che restano intatti e con la loro semantica:

| Concetto | Tabelle | Semantica | Modulo |
|---|---|---|---|
| Gruppo di allarme | `alarm_groups` / `alarm_group_members` | escalation/reperibilità (posizione = ladder) | Sicurezza |
| Squadra | `teams` / `team_members` | organizzazione operativa | Workforce |
| Profilo di protezione | `protection_profiles` / `protection_profile_members` | policy safety (max 1 per lavoratore) | Sicurezza |
| **Gruppo di persone** | **`user_groups` / `user_group_members`** | **insieme riusabile, nessun comportamento** | **core (comune)** |

## Modello dati (mig `0072_user_groups.sql`)

- `app.user_groups(id, tenant_id, name, kind, labels jsonb, created_at)` — `UNIQUE(tenant_id,name)`.
  `kind` libero (squadra/reparto/zona/turno…), `labels` per metadati futuri.
- `app.user_group_members(tenant_id, group_id, user_id, added_at)` — PK `(group_id,user_id)`.
- **RLS** `tenant_isolation` su entrambe + `GRANT … TO app_rt`. FK `ON DELETE CASCADE`
  (cancellare un gruppo svuota i suoi membri; cancellare un utente lo toglie dai gruppi).

## API (tutte `tenant_admin`, controllo lato backend `admin_only`)

| Metodo | Rotta | Azione |
|---|---|---|
| GET  | `/api/user-groups` | elenco gruppi + membri |
| POST | `/api/user-groups` | crea `{name, kind?, labels?}` |
| POST | `/api/user-groups/:id` | rinomina/aggiorna `{name, kind?, labels?}` |
| DELETE | `/api/user-groups/:id` | elimina (cascade membri) |
| POST | `/api/user-groups/:id/members` | aggiunge `{user_id}` (idempotente) |
| DELETE | `/api/user-groups/:id/members/:uid` | rimuove |
| POST | `/api/groups/:id/members/import` | **importa** i membri di un gruppo condiviso in un **gruppo di allarme** `{user_group_id}` |

L'import dimostra il riuso **senza accoppiare i modelli**: legge `user_group_members` e inserisce
in `alarm_group_members` con `position` progressiva dopo gli attuali membri,
`ON CONFLICT (group_id,user_id) DO NOTHING` → **idempotente**. Richiede un modulo escalation
attivo (`loneworker|traccar|iot|thingsboard`), altrimenti **403**. È **unidirezionale e
disaccoppiato**: eliminare il gruppo condiviso non tocca il gruppo di allarme già popolato.

## Isolamento e permessi

- **Tenant**: RLS impedisce a un tenant di vedere/modificare i gruppi di un altro (list non li
  mostra; update/delete cross-tenant → 404).
- **Ruolo**: solo `tenant_admin`; altri ruoli → 403.

## Prove (e2e `qa/user-groups-e2e.sh`, 16/16)

CRUD, add/del membri, import nel gruppo allarme (+ idempotenza), isolamento acme↔globex
(list/update/delete), permesso non-admin, disaccoppiamento alla cancellazione.

## Compatibilità / rollback

- **Additiva**: nessuna rotta o comportamento esistente referenzia `user_groups`; le suite di
  regressione (loneworker, escalation, campo) restano verdi, instradamento allarmi compreso.
- **Spegnimento via flag**: la primitiva è core (sempre viva); il *consumo* lato allarmi è gated
  dal modulo escalation → disattivandolo l'import risponde 403 e i gruppi di allarme esistenti
  restano invariati.
- **Build precedente su DB migrato**: `sqlx` è forward-only → un binario senza la `0072` **non
  parte** (`migration 72 … missing in the resolved migrations`). È il comportamento standard di
  tutte le migrazioni: il rollback previsto è **via flag** (preserva i dati), non il downgrade
  del binario.

## Roadmap (progettate, NON implementate)

- **Fase 2 — Capability layer**: permessi come capability (`can(cap)`) mappate oggi sui ruoli,
  per sganciare le autorizzazioni dai 7 ruoli fissi.
- **Fase 3 — Ruoli personalizzati per tenant**: ogni tenant definisce ruoli propri = insiemi di
  capability, assegnabili a utenti e/o a questi gruppi di persone.
