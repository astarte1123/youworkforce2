# Gruppi di persone come consumatori in Campo (Tappa 3)

I **gruppi di persone condivisi** (primitiva comune di Fase 1, `user_groups` — vedi
`gruppi-condivisi.md`) possono essere destinatari del lavoro sul campo: un **ordine di lavoro**
o un **giro di ronda** si assegna a un gruppo, e tutti i suoi membri lo vedono ed eseguono.
Additivo e non distruttivo: chi assegna al singolo utente continua a lavorare come prima.

## Dati

mig `0076_field_group_assignment.sql`: colonna nullable `assigned_group uuid REFERENCES
user_groups(id) ON DELETE SET NULL` su `work_orders` e `patrol_routes`, più indici parziali.
Default NULL = comportamento storico. Eliminare un gruppo **non** cancella ordini/giri: li
sgancia soltanto (SET NULL).

## Ordini di lavoro

- **Creazione/modifica** (`POST /orders`, `/orders/:id/edit`): campo opzionale `assigned_group`
  (e `clear_group` in edit). Un ordine con assegnatario (utente **o** gruppo) passa a `assegnato`.
- **Assegna a gruppo**: `POST /orders/:id/assign-group {version, group_id}` (solo pianificatore).
- **ACL esecuzione** (`order_edit_allowed`): consentita a pianificatore **o** assegnatario
  **o** membro del gruppo assegnato. Vale per arrivo/avvio/rapporto/allegati.
- **Visibilità** (`GET /orders`): il `lavoratore` vede i propri ordini **e** quelli assegnati a
  un gruppo di cui è membro. Pianificatore/admin/auditor vedono tutto (come prima).
- La risposta espone `assigned_group` e `group_name`.

## Giri / ronde

- **Route** (`POST`/`PUT /patrol-routes`): campo opzionale `assigned_group` (+ `clear_group`).
- **Visibilità** (`GET /patrol-routes`): il pianificatore vede tutti i giri; gli altri vedono i
  giri **senza gruppo** (aperti, come prima) e quelli del **proprio gruppo**.
- **Esecuzione** (`POST /patrol-routes/:id/rounds`): se il giro ha un gruppo, solo i membri (o il
  pianificatore) possono avviare la ronda; altrimenti 403. Giri senza gruppo: aperti a tutti.

## Console / PWA

- Console: i form **Nuovo/Modifica ordine** e **Nuovo giro** hanno il selettore «Assegna a gruppo
  di persone»; la lista ordini e la lista giri mostrano il badge 👥 col nome del gruppo.
- PWA «Oggi»: eredita automaticamente (le liste filtrano già per appartenenza al gruppo).

## Permessi

Assegnare = pianificatore (`plan`); eseguire = membro del gruppo. I gruppi si gestiscono in
**Utenti e ruoli** (capability `users.manage`). Nessun permesso nuovo introdotto.

## Prove

`qa/field-group-e2e.sh` — **13/13**: ordine/giro assegnati a gruppo, visibilità membro vs
non-membro, ACL esecuzione (arrive 200/403, round 200/403), `assign-group` (planner 200 /
non-planner 403), e giro **senza** gruppo aperto a tutti (default preservato). In CORE di
`qa/run-all.sh`. Regressione `MODE=core` 41/41 (2 fail noti ambientali crypto/outbox).
