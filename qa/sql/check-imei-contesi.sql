-- Controllo IN SOLA LETTURA: IMEI/identificativi GPS contesi tra tenant.
-- Da eseguire su qualunque ambiente (prima o dopo 0127/0128) con un ruolo che vede tutti i tenant:
--   psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -f qa/sql/check-imei-contesi.sql
-- Un IMEI conteso viene RIFIUTATO dal bridge Teltonika (log "identità ambigua"): nessuno dei tenant
-- riceve più dati né SOS da quel badge finché un operatore non elimina i doppioni sbagliati.
-- Colonna `monitorato`: il badge protegge oggi un lavoratore (in servizio, assegnato, abilitato,
-- modulo gps attivo). Su un DB senza 0128 lo stato di servizio non esiste: si usa la regola
-- precedente (assegnato + abilitato).
BEGIN TRANSACTION READ ONLY;

WITH has_service AS (
  SELECT EXISTS (SELECT 1 FROM information_schema.columns
                  WHERE table_schema='app' AND table_name='devices' AND column_name='service_state') AS v
), contested AS (
  SELECT external_id FROM app.devices WHERE source IN ('gps','traccar')
   GROUP BY external_id HAVING count(DISTINCT tenant_id) > 1
)
SELECT d.external_id                                   AS imei,
       t.slug                                          AS tenant,
       d.id                                            AS device_id,
       d.label,
       u.email                                         AS lavoratore,
       COALESCE(d.enabled, true)                       AS abilitato,
       CASE WHEN (SELECT v FROM has_service) THEN to_jsonb(d)->>'service_state' ELSE 'n/d (pre-0128)' END AS stato_servizio,
       (m.state = 'attivo')                            AS modulo_gps_attivo,
       (d.assigned_user IS NOT NULL AND COALESCE(d.enabled,true) AND m.state = 'attivo'
        AND (NOT (SELECT v FROM has_service) OR to_jsonb(d)->>'service_state' = 'in_servizio')) AS monitorato,
       to_jsonb(d)->>'last_seen_at'                    AS ultimo_contatto,   -- assente prima di 0125
       to_jsonb(d)->>'last_pos_at'                     AS ultima_posizione,
       d.created_at
  FROM contested c
  JOIN app.devices d ON d.external_id = c.external_id AND d.source IN ('gps','traccar')
  JOIN app.tenants t ON t.id = d.tenant_id
  LEFT JOIN app.users u ON u.id = d.assigned_user
  LEFT JOIN app.tenant_modules m ON m.tenant_id = d.tenant_id AND m.module_code IN ('gps','traccar')
 ORDER BY d.external_id, d.created_at;

-- riepilogo: quanti IMEI contesi e quanti con almeno un lavoratore che risulterebbe protetto
WITH has_service AS (
  SELECT EXISTS (SELECT 1 FROM information_schema.columns
                  WHERE table_schema='app' AND table_name='devices' AND column_name='service_state') AS v
)
SELECT count(*)                                       AS imei_contesi,
       count(*) FILTER (WHERE monitorati > 0)         AS con_lavoratori_monitorati
  FROM (SELECT d.external_id,
               count(*) FILTER (WHERE d.assigned_user IS NOT NULL AND COALESCE(d.enabled,true) AND m.state='attivo'
                 AND (NOT (SELECT v FROM has_service) OR to_jsonb(d)->>'service_state' = 'in_servizio')) AS monitorati
          FROM app.devices d
          LEFT JOIN app.tenant_modules m ON m.tenant_id = d.tenant_id AND m.module_code IN ('gps','traccar')
         WHERE d.source IN ('gps','traccar')
         GROUP BY d.external_id HAVING count(DISTINCT d.tenant_id) > 1) x;

ROLLBACK;
