-- Fase 4 Tappa 3: gruppi di persone (Fase 1, user_groups) come CONSUMATORI in Campo.
-- Assegnazione di ordini di lavoro e giri a un GRUPPO (oltre al singolo utente), con ACL per gruppo.
-- Additivo e non distruttivo: colonna nullable, default NULL = comportamento storico invariato.
-- ON DELETE SET NULL: eliminare un gruppo condiviso NON cancella ordini/giri, li sgancia soltanto.

ALTER TABLE work_orders   ADD COLUMN IF NOT EXISTS assigned_group uuid REFERENCES user_groups(id) ON DELETE SET NULL;
ALTER TABLE patrol_routes ADD COLUMN IF NOT EXISTS assigned_group uuid REFERENCES user_groups(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_work_orders_assigned_group   ON work_orders(assigned_group)   WHERE assigned_group IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_patrol_routes_assigned_group ON patrol_routes(assigned_group) WHERE assigned_group IS NOT NULL;
