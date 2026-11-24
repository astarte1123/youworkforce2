-- 0127 — Correzioni dati dopo 0125/0126 (rimozione connettori). 0125 e 0126 restano INTATTE
-- (checksum sqlx immutabili): tutto ciò che serve a correggerne gli effetti sta qui.
--
-- Gira all'avvio del core PRIMA che partano worker, bridge Teltonika e scansione escalation
-- (main.rs: migrate -> spawn worker), quindi nessun watchdog vede lo stato intermedio.

-- 1) Watchdog del silenzio armato dal deploy --------------------------------------------------------
-- 0125 ha seminato last_seen_at con l'ultima posizione: un badge in un cassetto da giorni risultava
-- "silenzioso" già al primo giro del watchdog -> allarmi critici in massa all'upgrade. Il watchdog
-- conta ora il silenzio da GREATEST(last_seen_at, silence_armed_at): per i dispositivi esistenti
-- l'armamento è il momento dell'upgrade, per i successivi il momento dell'assegnazione.
-- Gli incidenti perdita_contatto GIÀ aperti non si toccano: la loro origine non è verificabile.
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS silence_armed_at timestamptz;
UPDATE app.devices SET silence_armed_at = now() WHERE silence_armed_at IS NULL;

-- (Ri)assegnare un dispositivo a un lavoratore riarma il conteggio: un badge preso dal magazzino non
-- deve risultare "silenzioso da giorni" nel momento in cui viene consegnato.
CREATE OR REPLACE FUNCTION app.devices_arm_silence() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    NEW.silence_armed_at := COALESCE(NEW.silence_armed_at, now());
  ELSIF NEW.assigned_user IS DISTINCT FROM OLD.assigned_user THEN
    NEW.silence_armed_at := now();
  END IF;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS devices_arm_silence ON app.devices;
CREATE TRIGGER devices_arm_silence BEFORE INSERT OR UPDATE OF assigned_user ON app.devices
  FOR EACH ROW EXECUTE FUNCTION app.devices_arm_silence();

-- 2) Identità GPS univoca tra tenant ------------------------------------------------------------------
-- Il bridge Teltonika identifica il device SOLO dall'IMEI (il protocollo non porta token). Un IMEI
-- censito in due tenant rende l'identità contesa: il bridge la rifiuta (api::teltonika_resolve) e qui
-- se ne impediscono di NUOVE. Non è un indice UNIQUE perché eventuali duplicati preesistenti farebbero
-- fallire l'upgrade: restano rifiutati dal bridge finché un operatore non li risolve.
-- SECURITY DEFINER: deve vedere le righe di TUTTI i tenant (l'RLS del ruolo applicativo le nasconde).
CREATE OR REPLACE FUNCTION app.devices_gps_identity_unique() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, pg_temp AS $$
BEGIN
  -- un UPDATE che non cambia l'identità (es. modifica etichetta) non va bloccato, nemmeno su un
  -- duplicato preesistente
  IF TG_OP = 'UPDATE' AND NEW.external_id = OLD.external_id AND NEW.source = OLD.source
     AND NEW.tenant_id = OLD.tenant_id THEN
    RETURN NEW;
  END IF;
  IF NEW.source = 'gps' THEN
    PERFORM pg_advisory_xact_lock(hashtext('yw-gps-identity:' || NEW.external_id));
    IF EXISTS (SELECT 1 FROM app.devices d
                WHERE d.source = 'gps' AND d.external_id = NEW.external_id
                  AND d.tenant_id <> NEW.tenant_id AND d.id <> NEW.id) THEN
      RAISE EXCEPTION 'identificativo dispositivo GPS già registrato'
        USING ERRCODE = 'unique_violation';
    END IF;
  END IF;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS devices_gps_identity_unique ON app.devices;
CREATE TRIGGER devices_gps_identity_unique BEFORE INSERT OR UPDATE OF external_id, source, tenant_id ON app.devices
  FOR EACH ROW EXECUTE FUNCTION app.devices_gps_identity_unique();

-- 3) Incidenti "sistema di tracciamento irraggiungibile" (traccar_conn_down) ancora aperti -----------
-- Li chiudeva il watchdog della connessione Traccar, che non esiste più: resterebbero aperti e in
-- escalation per sempre. Si chiudono con esito esplicito e traccia in timeline; la loro escalation
-- si ferma (stato terminale). Lo storico resta leggibile (etichetta mantenuta nel codice).
WITH closed AS (
  UPDATE app.incidents
     SET state = 'chiuso', closed_at = now(), version = version + 1,
         outcome = 'Chiuso automaticamente: il collegamento al server Traccar è stato dismesso (YouWorkforce riceve i dispositivi in modo nativo)'
   WHERE etype = 'traccar_conn_down' AND state <> 'chiuso'
  RETURNING id, tenant_id
), tl AS (
  INSERT INTO app.incident_timeline(tenant_id, incident_id, kind, detail)
  SELECT tenant_id, id, 'transition',
         jsonb_build_object('to','chiuso','by','sistema (migrazione 0127)','reason','connettore_ritirato')
    FROM closed
)
INSERT INTO app.esc_alert_state(tenant_id, incident_id, step, step_fired_at, status)
SELECT tenant_id, id, 0, now(), 'closed' FROM closed
ON CONFLICT (tenant_id, incident_id) DO UPDATE SET status = 'closed', updated_at = now();

-- 4) Passaggio da GoAlert / motore "in ombra" all'escalation solo nativa -----------------------------
-- Prima di 0126 un tenant con motore esterno faceva avanzare la scala nativa IN OMBRA: i gradini
-- risultavano sparati (esc_alert_state) e "consegnati" solo al sink di prova (esc_delivery_log +
-- timeline esc_delivery con engine<>'native' e live=false). Chi avvisava davvero era GoAlert
-- (timeline goalert_sent, oggi notify_dispatched senza engine), ritirato il 19/11 (b6ed821).
-- Dopo 0126 il motore nativo riprende dallo stato registrato e considera consegnato ogni gradino
-- che ha una riga in esc_delivery_log. Per gli incidenti APERTI e NON presi in carico:
--   a) gradini solo in ombra, NESSUNA consegna reale e nessuna accettazione GoAlert -> nessuno è
--      mai stato avvisato: la scala riparte dal primo gradino. Si eliminano stato e righe di
--      consegna in ombra (copiate in timeline per l'audit), altrimenti l'idempotenza le
--      tratterebbe come già consegnate.                                        [niente perdite]
--   b) almeno una consegna reale (live=true) -> stato e log restano: il motore prosegue dal
--      gradino successivo senza ripetere quelli già consegnati.               [niente duplicati]
--   c) accettato da GoAlert ma senza stato nativo -> non si tocca. Il motore nativo consegnerà
--      il primo gradino: un possibile doppione del primo avviso, preferito alla perdita
--      dell'escalation. Si annota in timeline ('escalation_migrated', action=review).
CREATE TEMP TABLE _esc_open ON COMMIT DROP AS
SELECT i.id, i.tenant_id,
       eas.status AS esc_status,
       EXISTS (SELECT 1 FROM app.incident_timeline t
                WHERE t.incident_id = i.id AND t.kind = 'esc_delivery'
                  AND COALESCE(t.detail->>'engine','native') <> 'native'
                  AND COALESCE(t.detail->>'live','false') <> 'true') AS shadow_rungs,
       EXISTS (SELECT 1 FROM app.incident_timeline t
                WHERE t.incident_id = i.id AND t.kind = 'esc_delivery'
                  AND COALESCE(t.detail->>'live','false') = 'true') AS live_rungs,
       (SELECT min(t.at) FROM app.incident_timeline t
         WHERE t.incident_id = i.id AND t.kind = 'notify_dispatched'
           AND t.detail->>'engine' IS NULL AND NOT (t.detail ? 'outbox_kind')) AS goalert_accepted_at
  FROM app.incidents i
  LEFT JOIN app.esc_alert_state eas ON eas.tenant_id = i.tenant_id AND eas.incident_id = i.id
 WHERE i.state = 'aperto'
   AND COALESCE(eas.status,'new') NOT IN ('ack','closed');

-- a) solo ombra, mai consegnato -> riparte dal primo gradino
INSERT INTO app.incident_timeline(tenant_id, incident_id, kind, detail)
SELECT o.tenant_id, o.id, 'escalation_migrated',
       jsonb_build_object('action','restart',
         'reason','gradini consegnati solo in ombra (motore esterno): nessun destinatario avvisato',
         'previous_status', o.esc_status,
         'shadow_deliveries', COALESCE((SELECT jsonb_agg(jsonb_build_object('step',l.step,'channel',l.channel,
              'to',l.to_value,'attempt',l.attempt,'state',l.state,'at',l.at) ORDER BY l.at)
            FROM app.esc_delivery_log l WHERE l.tenant_id = o.tenant_id AND l.incident_id = o.id), '[]'::jsonb))
  FROM _esc_open o
 WHERE o.shadow_rungs AND NOT o.live_rungs AND o.goalert_accepted_at IS NULL;
DELETE FROM app.esc_delivery_log l USING _esc_open o
 WHERE l.tenant_id = o.tenant_id AND l.incident_id = o.id
   AND o.shadow_rungs AND NOT o.live_rungs AND o.goalert_accepted_at IS NULL;
DELETE FROM app.esc_alert_state e USING _esc_open o
 WHERE e.tenant_id = o.tenant_id AND e.incident_id = o.id
   AND o.shadow_rungs AND NOT o.live_rungs AND o.goalert_accepted_at IS NULL;

-- c) accettato da GoAlert, nessuno stato nativo: solo annotazione (vedi sopra)
INSERT INTO app.incident_timeline(tenant_id, incident_id, kind, detail)
SELECT tenant_id, id, 'escalation_migrated',
       jsonb_build_object('action','review',
         'reason','primo avviso consegnato da GoAlert; l''escalation nativa riparte dal primo gradino (possibile doppione del primo avviso)',
         'goalert_accepted_at', goalert_accepted_at)
  FROM _esc_open
 WHERE esc_status IS NULL AND goalert_accepted_at IS NOT NULL;
