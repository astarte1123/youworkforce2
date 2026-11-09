-- Rimuove il modulo sperimentale 'blues': stub mai implementato (nessun adapter
-- Notehub, nessun handler di ingress). Si riaggiungerà con un connettore vero
-- quando/se arriverà l'hardware Blues. Vedi anche CONNECTOR_MODS in core/src/api.rs.
DELETE FROM tenant_modules  WHERE module_code = 'blues';
DELETE FROM module_tools    WHERE module_code = 'blues';   -- difensivo: blues non ha tool
DELETE FROM modules_catalog WHERE code = 'blues';
