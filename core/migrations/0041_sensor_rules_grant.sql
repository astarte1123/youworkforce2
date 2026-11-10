-- La 0040 ha creato app.sensor_rules ma senza i privilegi per il ruolo RLS app_rt.
GRANT SELECT, INSERT, UPDATE, DELETE ON app.sensor_rules TO app_rt;
