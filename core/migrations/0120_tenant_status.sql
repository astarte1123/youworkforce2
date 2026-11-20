-- Ciclo di vita del tenant (governance superadmin): attivo | sospeso.
-- 'sospeso' blocca il login degli utenti del tenant (l'impersonazione superadmin per assistenza resta).
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS status text NOT NULL DEFAULT 'attivo';
