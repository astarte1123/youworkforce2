-- Il superadmin deve poter gestire i tenant anche sul piano amministrativo:
-- numero di licenze (seat app mobile + dispositivi IoT), identita' visiva
-- (logo + colori principali) e impersonazione dell'admin per supporto.

-- Licenze (NULL = illimitato). Policy soft: si traccia e si avvisa, non si blocca.
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS mobile_app_licenses  int;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS iot_device_licenses  int;

-- Branding per-tenant (NULL = usa il default globale YouCo). Colori come stringhe CSS.
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS brand_primary text;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS brand_ink     text;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS brand_soft    text;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS brand_accent  text;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS logo       bytea;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS logo_ct    text;

-- Impersonazione: la sessione creata dal superadmin per assistere un tenant porta
-- l'email del superadmin che la ha generata. Serve al banner UI e all'audit, e
-- consente lo "stop" che riconia la sessione superadmin senza nuovo login.
ALTER TABLE app.sessions ADD COLUMN IF NOT EXISTS impersonator text;
