-- Configurazione monitoring dal pannello superadmin (invece che solo da env): soglie + destinatari
-- alert. Riga singola (id=true). NULL su un campo = usa il default (env/prodotto). Control-plane.
CREATE TABLE IF NOT EXISTS app.system_settings (
    id               boolean PRIMARY KEY DEFAULT true CHECK (id),
    disk_max_pct     int,
    mem_max_pct      int,
    cpu_max_pct      int,
    worker_stale_secs int,
    alert_email      text,   -- destinatari extra (uno o più, separati da virgola)
    alert_phone      text,   -- numero per SMS (E.164)
    updated_at       timestamptz NOT NULL DEFAULT now()
);
INSERT INTO app.system_settings(id) VALUES (true) ON CONFLICT (id) DO NOTHING;
