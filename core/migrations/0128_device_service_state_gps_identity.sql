-- 0128 — Stato di servizio esplicito dei badge e identità GPS indipendente dall'isolamento.
-- 0127 resta intatta (già applicata): qui se ne sostituiscono i due trigger.
--
-- 1) STATO DI SERVIZIO ---------------------------------------------------------------------------
-- Prima l'assegnazione a un lavoratore valeva anche come messa in servizio: il conteggio del
-- silenzio partiva dall'assegnazione e un badge che non aveva MAI trasmesso era escluso dal
-- watchdog (nessun allarme anche se il lavoratore lo portava spento). Ora:
--   in_servizio    il badge protegge il lavoratore assegnato: il watchdog è attivo. Il conteggio
--                  parte dalla MESSA IN SERVIZIO (azione esplicita) e riparte a ogni passaggio di
--                  consegne a un altro lavoratore; scatta alla soglia anche senza primo contatto.
--   in_attesa      censito/assegnato ma non ancora messo in servizio: visibile, nessun allarme.
--   fuori_servizio ritirato (guasto, magazzino, disabilitato): visibile, nessun allarme.
-- Regole automatiche (trigger): senza lavoratore assegnato un badge in servizio torna in_attesa;
-- disabilitato passa fuori_servizio. Rimettere in servizio è sempre un'azione esplicita.
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS service_state text NOT NULL DEFAULT 'in_attesa';
ALTER TABLE app.devices DROP CONSTRAINT IF EXISTS devices_service_state_chk;
ALTER TABLE app.devices ADD CONSTRAINT devices_service_state_chk
  CHECK (service_state IN ('in_servizio','in_attesa','fuori_servizio'));

-- Classificazione dei badge esistenti. Il monitoraggio di chi era già monitorato si CONSERVA:
-- in_servizio con l'armamento di 0127 invariato (nessun nuovo azzeramento del conteggio).
-- Un badge assegnato che non ha mai trasmesso non era monitorato: resta in_attesa finché qualcuno
-- non lo mette in servizio (altrimenti l'upgrade aprirebbe subito allarmi per badge mai accesi).
UPDATE app.devices SET service_state = CASE
    WHEN NOT COALESCE(enabled, true)                                THEN 'fuori_servizio'
    WHEN assigned_user IS NOT NULL AND last_seen_at IS NOT NULL     THEN 'in_servizio'
    ELSE 'in_attesa' END;
UPDATE app.devices SET silence_armed_at = now() WHERE service_state = 'in_servizio' AND silence_armed_at IS NULL;

CREATE OR REPLACE FUNCTION app.devices_arm_silence() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  -- transizioni automatiche: niente portatore o device disabilitato = non può proteggere nessuno
  IF NOT COALESCE(NEW.enabled, true) AND NEW.service_state = 'in_servizio' THEN
    NEW.service_state := 'fuori_servizio';
  ELSIF NEW.assigned_user IS NULL AND NEW.service_state = 'in_servizio' THEN
    NEW.service_state := 'in_attesa';
  END IF;
  -- L'applicazione passa l'istante dal proprio orologio (st.clock, unico per la logica); se una
  -- scrittura non lo fa (script, altri percorsi) si usa now() del DB, identico in produzione.
  IF TG_OP = 'INSERT' THEN
    IF NEW.service_state = 'in_servizio' THEN NEW.silence_armed_at := COALESCE(NEW.silence_armed_at, now()); END IF;
  ELSIF NEW.service_state = 'in_servizio'
        AND (OLD.service_state <> 'in_servizio' OR NEW.assigned_user IS DISTINCT FROM OLD.assigned_user)
        AND NEW.silence_armed_at IS NOT DISTINCT FROM OLD.silence_armed_at THEN
    NEW.silence_armed_at := now();                          -- messa in servizio / passaggio di consegne
  END IF;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS devices_arm_silence ON app.devices;
CREATE TRIGGER devices_arm_silence BEFORE INSERT OR UPDATE OF assigned_user, service_state, enabled ON app.devices
  FOR EACH ROW EXECUTE FUNCTION app.devices_arm_silence();

-- 2) IDENTITÀ GPS (IMEI) UNICA TRA TENANT, A PROVA DI CONCORRENZA -------------------------------
-- 0127 usava lock advisory + EXISTS: corretto in READ COMMITTED (ogni istruzione vede i commit
-- concorrenti), ma in REPEATABLE READ/SERIALIZABLE la seconda transazione non vede la prima e il
-- duplicato passerebbe. Qui la titolarità è una riga con CHIAVE PRIMARIA: il vincolo di unicità
-- vale con qualunque isolamento (in RR un conflitto non visibile solleva un errore di
-- serializzazione, mai un duplicato).
--   tenant_id = titolare; contested = IMEI già conteso tra tenant prima di 0127 (nessun titolare,
--   nessuna nuova registrazione possibile finché un operatore non risolve).
CREATE TABLE IF NOT EXISTS app.gps_identity (
  external_id text PRIMARY KEY,
  tenant_id   uuid,
  contested   boolean NOT NULL DEFAULT false,
  updated_at  timestamptz NOT NULL DEFAULT now(),
  CHECK (contested OR tenant_id IS NOT NULL)
);
INSERT INTO app.gps_identity(external_id, tenant_id, contested)
SELECT external_id,
       CASE WHEN count(DISTINCT tenant_id) = 1 THEN min(tenant_id::text)::uuid END,
       count(DISTINCT tenant_id) > 1
  FROM app.devices WHERE source = 'gps' GROUP BY external_id
ON CONFLICT (external_id) DO NOTHING;

-- ricalcolo della titolarità di un IMEI dai device esistenti (dopo cancellazioni/spostamenti)
CREATE OR REPLACE FUNCTION app.gps_identity_refresh(ext text) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, pg_temp AS $$
DECLARE n int; t uuid;
BEGIN
  SELECT count(DISTINCT tenant_id), min(tenant_id::text)::uuid INTO n, t
    FROM app.devices WHERE source = 'gps' AND external_id = ext;
  IF n = 0 THEN DELETE FROM app.gps_identity WHERE external_id = ext;
  ELSIF n = 1 THEN UPDATE app.gps_identity SET tenant_id = t, contested = false, updated_at = now() WHERE external_id = ext;
  ELSE UPDATE app.gps_identity SET tenant_id = NULL, contested = true, updated_at = now() WHERE external_id = ext;
  END IF;
END $$;

CREATE OR REPLACE FUNCTION app.devices_gps_identity_unique() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, pg_temp AS $$
DECLARE owner uuid; is_contested boolean;
BEGIN
  IF TG_OP = 'UPDATE' AND NEW.external_id = OLD.external_id AND NEW.source = OLD.source
     AND NEW.tenant_id = OLD.tenant_id THEN
    RETURN NEW;                                   -- identità invariata (es. etichetta)
  END IF;
  IF NEW.source = 'gps' THEN
    INSERT INTO app.gps_identity(external_id, tenant_id) VALUES (NEW.external_id, NEW.tenant_id)
      ON CONFLICT (external_id) DO NOTHING;
    SELECT tenant_id, contested INTO owner, is_contested
      FROM app.gps_identity WHERE external_id = NEW.external_id FOR UPDATE;
    IF is_contested OR owner IS DISTINCT FROM NEW.tenant_id THEN
      RAISE EXCEPTION 'identificativo dispositivo GPS già registrato' USING ERRCODE = 'unique_violation';
    END IF;
  END IF;
  RETURN NEW;
END $$;

-- rilascio della titolarità quando un device GPS sparisce o cambia identità
CREATE OR REPLACE FUNCTION app.devices_gps_identity_release() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, pg_temp AS $$
BEGIN
  IF OLD.source = 'gps' AND (TG_OP = 'DELETE' OR NEW.external_id <> OLD.external_id
       OR NEW.source <> OLD.source OR NEW.tenant_id <> OLD.tenant_id) THEN
    PERFORM app.gps_identity_refresh(OLD.external_id);
  END IF;
  RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS devices_gps_identity_release ON app.devices;
CREATE TRIGGER devices_gps_identity_release AFTER DELETE OR UPDATE OF external_id, source, tenant_id ON app.devices
  FOR EACH ROW EXECUTE FUNCTION app.devices_gps_identity_release();
