# Ruoli personalizzati per tenant — Fase 3

Un tenant definisce **ruoli propri** = bundle **nominati** di capability (Fase 2), assegnabili a
**utenti** e/o ai **gruppi di persone condivisi** (Fase 1). **Solo additivi**: aggiungono
capability sopra il ruolo base dell'utente, non le tolgono. Additivo e non distruttivo: senza
assegnazioni il comportamento resta identico (ruoli fissi + capability layer).

## Come si combinano le capability (per utente)

```
caps(utente) = default_role_caps(ruolo_base) ± override_tenant      (Fase 2, livello ruolo)
             ∪ capability dei ruoli custom assegnati all'utente      (Fase 3, diretti)
             ∪ capability dei ruoli custom assegnati ai suoi gruppi  (Fase 3, via gruppi condivisi)
```

Calcolate in `effective_caps_user(tenant, ruolo, user_id)` a **ogni richiesta**: assegnare o
togliere un ruolo custom ha **effetto immediato**, senza re-login.

## Modello dati (mig `0074_custom_roles.sql`)

- `app.custom_roles(id, tenant_id, name, description, created_at)` — UNIQUE(tenant_id,name).
- `app.custom_role_capabilities(tenant_id, role_id, capability)` — PK(role_id,capability).
- `app.user_custom_roles(tenant_id, user_id, role_id)` — PK(user_id,role_id).
- `app.user_group_custom_roles(tenant_id, group_id, role_id)` — PK(group_id,role_id).

Tutte RLS `tenant_isolation` + GRANT app_rt. FK `ON DELETE CASCADE`: cancellare il ruolo rimuove
capability e assegnazioni; cancellare utente/gruppo rimuove le sue assegnazioni.

## API (tenant_admin)

| Metodo | Rotta | Azione |
|---|---|---|
| GET | `/api/custom-roles` | elenco ruoli + capability + assegnazioni (utenti/gruppi) + catalogo |
| POST | `/api/custom-roles` | crea `{name, description?, capabilities[]}` |
| POST | `/api/custom-roles/:id` | aggiorna nome/descrizione/capability (sostituisce il set) |
| DELETE | `/api/custom-roles/:id` | elimina (cascade) |
| POST | `/api/custom-roles/:id/users` | assegna `{user_id}` |
| DELETE | `/api/custom-roles/:id/users/:uid` | rimuove assegnazione utente |
| POST | `/api/custom-roles/:id/groups` | assegna `{group_id}` (gruppo condiviso) |
| DELETE | `/api/custom-roles/:id/groups/:gid` | rimuove assegnazione gruppo |

Validazione: capability fuori catalogo → 400; nome duplicato → 409.

## UI

**Utenti e sedi → «Ruoli personalizzati»**: crea/modifica/elimina ruolo (nome, descrizione,
capability), assegna/rimuovi utenti e gruppi condivisi. `GET /api/me` espone le `caps` effettive.

## Isolamento e compatibilità

- **Tenant**: ruoli e assegnazioni isolati per RLS; un tenant non vede/assegna i ruoli di un altro.
- **Default identico**: senza ruoli custom il comportamento è invariato; regressione verde
  (capabilities 17, user-groups 16, loneworker 15, field-slice1 8).
- **Prove**: `qa/custom-roles-e2e.sh` **16/16** (create, assegna a utente e a gruppo con effetto
  immediato, additività non impatta altri, /api/me, rimozione, isolamento tenant, validazione,
  delete con cascade).

## Fasi correlate

- [Fase 1 — gruppi condivisi](gruppi-condivisi.md)
- [Fase 2 — capability layer](capability-layer.md)
