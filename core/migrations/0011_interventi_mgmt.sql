-- ============================================================================
-- Fase 3 — Interventi: pianificazione (finestra temporale) + priorita' sugli ordini.
-- Abilita agenda/calendario e modifica dell'ordine con concorrenza ottimistica.
-- ============================================================================
ALTER TABLE work_orders ADD COLUMN IF NOT EXISTS scheduled_start timestamptz;
ALTER TABLE work_orders ADD COLUMN IF NOT EXISTS scheduled_end   timestamptz;
ALTER TABLE work_orders ADD COLUMN IF NOT EXISTS priority        int NOT NULL DEFAULT 2;  -- 1 bassa, 2 media, 3 alta

CREATE INDEX IF NOT EXISTS work_orders_sched ON work_orders(tenant_id, scheduled_start);
