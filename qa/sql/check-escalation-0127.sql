-- Controllo IN SOLA LETTURA, da eseguire PRIMA di applicare 0127 su un ambiente di destinazione:
-- elenca gli incidenti aperti interessati dalla migrazione e l'esito previsto per ciascuno.
--   psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -f qa/sql/check-escalation-0127.sql
-- Ricalca le regole di 0127 §3-§4 senza scrivere nulla (transazione READ ONLY, niente tabelle
-- temporanee). Funziona anche su DB fermi prima di 0125/0126: riconosce i vecchi nomi della
-- timeline (goalert_sent, goalert_suppressed_native).
-- Esiti:
--   chiusura                 traccar_conn_down aperto: chiuso, escalation ferma, nessuna notifica
--   invariato_preso_in_carico già preso in carico o chiuso lato escalation: non toccato
--   riavvio_primo_gradino    solo consegne "in ombra", nessuno avvisato: la scala riparte (notifica nuova)
--   possibile_doppio_avviso  accettato da GoAlert, nessuno stato nativo: il nativo invia il primo
--                            gradino (possibile doppione del primo avviso, preferito alla perdita)
--   continuita               consegne reali o stato nativo: si prosegue dal gradino registrato
-- Su un DB dove 0127 è GIÀ applicata il risultato non è significativo (lo stato è già migrato).
BEGIN TRANSACTION READ ONLY;

WITH applied AS (
  SELECT EXISTS (SELECT 1 FROM app._sqlx_migrations WHERE version = 127) AS v
), inc AS (
  SELECT i.id, i.tenant_id, i.etype, i.state, i.opened_at, eas.status AS esc_status, eas.step AS esc_step,
         EXISTS (SELECT 1 FROM app.incident_timeline t
                  WHERE t.incident_id = i.id AND t.kind = 'esc_delivery'
                    AND COALESCE(t.detail->>'engine','native') <> 'native'
                    AND COALESCE(t.detail->>'live','false') <> 'true')                AS shadow_rungs,
         EXISTS (SELECT 1 FROM app.incident_timeline t
                  WHERE t.incident_id = i.id AND t.kind = 'esc_delivery'
                    AND COALESCE(t.detail->>'live','false') = 'true')                 AS live_rungs,
         (SELECT min(t.at) FROM app.incident_timeline t
           WHERE t.incident_id = i.id
             AND ((t.kind = 'notify_dispatched' AND t.detail->>'engine' IS NULL AND NOT (t.detail ? 'outbox_kind'))
                  OR t.kind = 'goalert_sent'))                                          AS goalert_accepted_at,
         (SELECT count(*) FROM app.esc_delivery_log l WHERE l.incident_id = i.id)       AS log_rows
    FROM app.incidents i
    LEFT JOIN app.esc_alert_state eas ON eas.tenant_id = i.tenant_id AND eas.incident_id = i.id
   WHERE i.state <> 'chiuso'
)
SELECT t.slug AS tenant, inc.id AS incident_id, inc.etype, inc.state, inc.opened_at,
       inc.esc_status, inc.esc_step, inc.shadow_rungs, inc.live_rungs, inc.goalert_accepted_at, inc.log_rows,
       CASE
         WHEN inc.etype = 'traccar_conn_down'                                    THEN 'chiusura'
         WHEN inc.state <> 'aperto' OR COALESCE(inc.esc_status,'new') IN ('ack','closed') THEN 'invariato_preso_in_carico'
         WHEN inc.shadow_rungs AND NOT inc.live_rungs AND inc.goalert_accepted_at IS NULL THEN 'riavvio_primo_gradino'
         WHEN inc.esc_status IS NULL AND inc.goalert_accepted_at IS NOT NULL      THEN 'possibile_doppio_avviso'
         ELSE 'continuita'
       END AS esito_previsto,
       CASE WHEN inc.etype = 'perdita_contatto' THEN 'resta aperto (origine non verificabile)' END AS nota,
       (SELECT v FROM applied) AS migrazione_0127_gia_applicata
  FROM inc JOIN app.tenants t ON t.id = inc.tenant_id
 ORDER BY esito_previsto, t.slug, inc.opened_at;

ROLLBACK;
