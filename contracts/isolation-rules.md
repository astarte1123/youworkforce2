# Regole di isolamento multitenant

Isolamento **end-to-end**, non solo filtro UI (Prompt 1).

## Risoluzione del tenant
- **Browser**: tenant dal dominio validato (`{slug}.loneworker.eu`) + appartenenza dell'utente autenticato. In locale: header `X-Tenant-Slug` (solo dev) o Host.
- **Macchina** (ingress): tenant dalla **credenziale** associata alla sorgente (`ingest_tokens`), **mai** dal payload.
- Il proxy fidato (console Go) inoltra header e cookie; nessun header/campo JSON scelto dal client vale come autorizzazione.

## Enforcement in database (PostgreSQL 18)
- Ruolo runtime `app_rt`: `LOGIN`, **`NOBYPASSRLS`**, non proprietario delle tabelle.
- RLS attiva su tutte le tabelle cliente; policy `tenant_id = current_setting('app.current_tenant')::uuid`.
- Il contesto tenant è legato alla **transazione** (`set_config(..., true)` = `SET LOCAL`) → nessuna contaminazione nel pool.
- Il worker di sistema usa la connessione **owner** (bypassa RLS di proposito) per drenare outbox e timer cross-tenant.
- FK e chiavi naturali includono `tenant_id`; `UNIQUE(tenant_id, source, idem_key)` per idempotenza.

## Cosa viene respinto (vedi T02)
- Credenziale di Acme che scrive per un secondo tenant → RLS `WITH CHECK` rifiuta.
- Utente estraneo con ID valido → nessuna riga visibile (policy `USING`).
- Webhook/device con token errato → `401`.
- SSE/liste/export: scope dalla sessione (tenant_id), non da parametri client.

## Superadmin
- Controllo commerciale/tecnico sul piano di controllo; **nessun accesso implicito** a posizioni, incidenti, utenti operativi del cliente. Le scritture di controllo passano dalla connessione owner, separate dai dati operativi.
