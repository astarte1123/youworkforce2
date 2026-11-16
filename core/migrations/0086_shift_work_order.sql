-- 0086 — Legame turno ↔ intervento/cantiere (Lavoro sul campo). Additiva.
-- Un turno può essere associato a un ordine di lavoro (intervento o cantiere). Legame DEBOLE:
-- ON DELETE SET NULL, così spegnere/rimuovere interventi non rompe i turni.
ALTER TABLE app.shift_plans ADD COLUMN IF NOT EXISTS work_order_id uuid REFERENCES app.work_orders(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS shift_plans_wo ON app.shift_plans(work_order_id) WHERE work_order_id IS NOT NULL;
