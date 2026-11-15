-- 0080 — Ponte intervento -> consuntivo (Slice 2). SOLO additiva.
-- Tracciamento della riga-sorgente per idempotenza: una riga "labor" per (ordine, utente).

ALTER TABLE app.timesheet_lines ADD COLUMN IF NOT EXISTS source_order_id uuid REFERENCES app.work_orders(id) ON DELETE SET NULL;
ALTER TABLE app.timesheet_lines ADD COLUMN IF NOT EXISTS source_user_id  uuid;
ALTER TABLE app.timesheet_lines ADD COLUMN IF NOT EXISTS source_kind     text;

-- Idempotenza: la generazione da ordine non duplica la riga labor dello stesso tecnico.
CREATE UNIQUE INDEX IF NOT EXISTS timesheet_lines_source_uq
    ON app.timesheet_lines(tenant_id, source_order_id, source_user_id, source_kind)
    WHERE source_order_id IS NOT NULL;
