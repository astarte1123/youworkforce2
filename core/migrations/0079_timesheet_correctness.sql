-- 0079 — Correttezza consuntivi (Slice 1). SOLO additiva.
-- Aggiunge il versioning ottimistico (fondamenta) e vincola gli stati ammessi.

-- Versione per optimistic lock / tracciamento mutazioni (default 1 sulle righe esistenti).
ALTER TABLE app.timesheets ADD COLUMN IF NOT EXISTS version integer NOT NULL DEFAULT 1;

-- Vincolo sugli stati ammessi (prima era text libero). Idempotente.
ALTER TABLE app.timesheets DROP CONSTRAINT IF EXISTS timesheets_state_chk;
ALTER TABLE app.timesheets ADD CONSTRAINT timesheets_state_chk
    CHECK (state IN ('bozza','in_revisione','da_correggere','approvato'));
