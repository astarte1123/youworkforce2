-- Turni di reperibilita' pilotati da YouWorkforce: un gruppo allarme puo' notificare
-- TUTTI i membri insieme (default) oppure a TURNI (rotazione). In modalita' turni
-- YouWorkforce provvede una rotation in GoAlert (membri a rotazione) e lo step della
-- escalation policy punta alla rotation: viene avvisato il reperibile di turno.
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS oncall_mode  text NOT NULL DEFAULT 'tutti';   -- 'tutti' | 'turni'
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS oncall_type  text NOT NULL DEFAULT 'weekly';   -- hourly|daily|weekly|monthly
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS oncall_shift int  NOT NULL DEFAULT 1;           -- lunghezza turno (unita' di oncall_type)
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS oncall_tz    text NOT NULL DEFAULT 'Europe/Rome';
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS goalert_rotation_id text;
