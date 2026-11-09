-- ============================================================================
-- Fase A — Catalogo modelli dispositivo (globale, curato).
-- Dato Traccar: modello -> protocollo -> porta. Categoria/capacita' curate da noi
-- per un sottoinsieme rilevante lone-worker/flotta. Fallback "Generico/OsmAnd".
-- ============================================================================
CREATE TABLE device_models (
  model_code     text PRIMARY KEY,
  manufacturer   text NOT NULL,
  model_name     text NOT NULL,
  protocol       text NOT NULL,
  default_port   int,
  category       text NOT NULL,              -- personale|veicolo|asset|smartphone|sensore
  capabilities   text[] NOT NULL DEFAULT '{}',
  osmand         boolean NOT NULL DEFAULT false,
  default_config jsonb NOT NULL DEFAULT '{}'::jsonb,
  recommended    boolean NOT NULL DEFAULT false,
  notes          text
);
GRANT SELECT ON app.device_models TO app_rt;

INSERT INTO device_models(model_code,manufacturer,model_name,protocol,default_port,category,capabilities,osmand,default_config,recommended,notes) VALUES
-- ---- Tracker personali (lone worker) ----
 ('teltonika-tmt250','Teltonika','TMT250','teltonika',5027,'personale',
    ARRAY['gps','battery','sos','fall','movement'],false,
    '{"sos_as":"sos","stopped_as_immobilita":"si","battery_min":15}',true,'Tracker personale con SOS e rilevamento caduta'),
 ('meitrack-p99g','Meitrack','P99G','meitrack',5020,'personale',
    ARRAY['gps','battery','sos','fall','two_way_audio'],false,
    '{"sos_as":"sos","stopped_as_immobilita":"si","battery_min":15}',false,'SOS + audio bidirezionale'),
 ('queclink-gl300w','Queclink','GL300W','gl200',5004,'personale',
    ARRAY['gps','battery','sos','geofence'],false,'{"sos_as":"sos","battery_min":20}',false,NULL),
 ('concox-gt03','Concox','GT03','gt06',5023,'personale',
    ARRAY['gps','battery','sos'],false,'{"sos_as":"sos","battery_min":15}',false,NULL),
 ('xexun-xt107','Xexun','XT107','xexun',5002,'personale',
    ARRAY['gps','battery','sos','fall'],false,'{"sos_as":"sos","stopped_as_immobilita":"si"}',false,NULL),
-- ---- Smartphone / app ----
 ('smartphone-osmand','Generico','Smartphone (OsmAnd)','osmand',5055,'smartphone',
    ARRAY['gps','battery','sos'],true,'{"sos_as":"sos","battery_min":20}',true,'App OsmAnd / Traccar Client sul telefono del lavoratore'),
-- ---- Veicoli ----
 ('teltonika-fmb920','Teltonika','FMB920','teltonika',5027,'veicolo',
    ARRAY['gps','ignition','battery','immobilizer','motion'],false,'{"battery_min":10}',true,NULL),
 ('teltonika-fmc130','Teltonika','FMC130','teltonika',5027,'veicolo',
    ARRAY['gps','ignition','battery','immobilizer','temperature'],false,'{"battery_min":10}',false,NULL),
 ('concox-gt06n','Concox','GT06N','gt06',5023,'veicolo',
    ARRAY['gps','ignition','battery'],false,'{"battery_min":10}',false,NULL),
 ('queclink-gv75','Queclink','GV75','gl200',5004,'veicolo',
    ARRAY['gps','ignition','battery','motion'],false,'{"battery_min":10}',false,NULL),
 ('ruptela-fmeco4','Ruptela','FM-Eco4','ruptela',5046,'veicolo',
    ARRAY['gps','ignition','battery'],false,'{"battery_min":10}',false,NULL),
-- ---- Asset / attrezzature ----
 ('digitalmatter-oyster3','Digital Matter','Oyster3','oyster',5203,'asset',
    ARRAY['gps','battery'],false,'{"battery_min":10}',true,'Lunga autonomia, asset non alimentati'),
 ('teltonika-fmb110','Teltonika','FMB110','teltonika',5027,'asset',
    ARRAY['gps','battery','motion'],false,'{"battery_min":10}',false,NULL),
 ('queclink-gl320m','Queclink','GL320M','gl200',5004,'asset',
    ARRAY['gps','battery','motion'],false,'{"battery_min":15}',false,NULL),
-- ---- Sensori / generico ----
 ('generic-osmand','Generico','Dispositivo generico (OsmAnd)','osmand',5055,'sensore',
    ARRAY['gps','battery'],true,'{"battery_min":15}',false,'Qualsiasi sorgente che parla il protocollo OsmAnd');
