-- 0129 — Modulo "gps" (Localizzazione GPS e badge) attivabile.
-- 0125 aveva creato la riga di catalogo copiando `available` dal vecchio modulo `traccar`, che la
-- 0078 aveva già marcato non disponibile (connettore ritirato): `gps` nasceva quindi NON attivabile
-- e il superadmin riceveva "modulo ritirato dal prodotto" su qualunque tenant (errore SA-1).
-- `gps` è un modulo NATIVO (bridge Teltonika, OsmAnd, JSON HTTP), non un connettore: va disponibile.
-- Idempotente; non tocca lo stato dei moduli dei tenant (chi lo aveva attivo resta attivo).
UPDATE app.modules_catalog
   SET available = true, name = 'Localizzazione GPS e badge', kind = 'product'
 WHERE code = 'gps';

INSERT INTO app.modules_catalog(code, name, kind, available)
SELECT 'gps', 'Localizzazione GPS e badge', 'product', true
 WHERE NOT EXISTS (SELECT 1 FROM app.modules_catalog WHERE code = 'gps');

-- Tenant creati quando `gps` non era nel catalogo: riga mancante → la si crea non abilitata, come fa
-- la creazione tenant per ogni modulo del catalogo (l'attivazione resta un'azione del superadmin).
INSERT INTO app.tenant_modules(tenant_id, module_code, state)
SELECT t.id, 'gps', 'non_abilitato' FROM app.tenants t
 WHERE NOT EXISTS (SELECT 1 FROM app.tenant_modules m WHERE m.tenant_id = t.id AND m.module_code = 'gps');
