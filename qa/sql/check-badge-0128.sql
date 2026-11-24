-- Controllo IN SOLA LETTURA: stato di servizio che ogni badge GPS avrà (o ha) dopo la 0128 e
-- l'azione richiesta al responsabile operativo. Da eseguire su qualunque ambiente, prima o dopo 0128:
--   psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -f qa/sql/check-badge-0128.sql
-- Prima di 0128 ricalca la classificazione della migrazione (stessa regola del backfill):
--   disabilitato                       -> fuori_servizio
--   assegnato + almeno un contatto     -> in_servizio (monitoraggio conservato)
--   altrimenti                         -> in_attesa   (nessun allarme finché non messo in servizio)
-- Dopo 0128 legge lo stato reale. Funziona anche prima di 0125 (nomi 'traccar', niente last_seen_at):
-- le colonne opzionali si leggono via to_jsonb, quindi nessun errore su schemi vecchi.
BEGIN TRANSACTION READ ONLY;

WITH dev AS (
  SELECT d.id, d.tenant_id, d.external_id, d.label, d.assigned_user, COALESCE(d.enabled,true) AS abilitato,
         COALESCE((to_jsonb(d)->>'last_seen_at')::timestamptz,
                  GREATEST((to_jsonb(d)->>'last_pos_at')::timestamptz,
                           (to_jsonb(d)->>'last_telemetry_at')::timestamptz))      AS ultimo_contatto,
         to_jsonb(d)->>'service_state'                                              AS stato_attuale
    FROM app.devices d
   WHERE d.source IN ('gps','traccar')
), contested AS (
  SELECT external_id FROM dev GROUP BY external_id HAVING count(DISTINCT tenant_id) > 1
), cls AS (
  SELECT dev.*,
         COALESCE(stato_attuale, CASE
           WHEN NOT abilitato                                             THEN 'fuori_servizio'
           WHEN assigned_user IS NOT NULL AND ultimo_contatto IS NOT NULL THEN 'in_servizio'
           ELSE 'in_attesa' END)                                          AS stato_dopo_0128,
         (external_id IN (SELECT external_id FROM contested))             AS imei_conteso
    FROM dev
)
SELECT t.slug AS tenant, cls.external_id AS imei, cls.label, u.email AS lavoratore,
       cls.abilitato, cls.ultimo_contatto, cls.stato_attuale, cls.stato_dopo_0128, cls.imei_conteso,
       CASE
         WHEN cls.imei_conteso THEN
           'IMEI conteso: decidere il tenant titolare, eliminare il doppione, poi verificare il primo contatto'
         WHEN cls.stato_dopo_0128 = 'in_attesa' AND cls.assigned_user IS NOT NULL THEN
           'Lavoratore NON protetto: accendere il badge, verificare il primo contatto nella scheda, poi "Metti in servizio"'
         WHEN cls.stato_dopo_0128 = 'in_attesa' THEN
           'Nessuna urgenza: assegnare al lavoratore alla consegna e poi "Metti in servizio"'
         WHEN cls.stato_dopo_0128 = 'fuori_servizio' AND cls.assigned_user IS NOT NULL THEN
           'Verificare: badge disabilitato ma assegnato. Se in uso, riabilitare e "Metti in servizio"; altrimenti togliere l''assegnazione'
         WHEN cls.stato_dopo_0128 = 'fuori_servizio' THEN 'Nessuna (ritirato)'
         WHEN cls.ultimo_contatto < now() - interval '1 day' THEN
           'In servizio ma silente da oltre un giorno: dopo l''upgrade scatterà perdita di contatto. Verificare il badge o metterlo fuori servizio'
         ELSE 'Nessuna (monitoraggio conservato)'
       END AS azione_responsabile
  FROM cls
  JOIN app.tenants t ON t.id = cls.tenant_id
  LEFT JOIN app.users u ON u.id = cls.assigned_user
 ORDER BY (cls.imei_conteso) DESC,
          (cls.stato_dopo_0128 = 'in_attesa' AND cls.assigned_user IS NOT NULL) DESC,
          t.slug, cls.external_id;

ROLLBACK;
