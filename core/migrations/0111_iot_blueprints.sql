-- 0111 — IoT Fase 2: Blueprint (profilo-lavoro). L'admin sceglie "che lavoro deve fare il device"
-- e l'attivazione configura TUTTO da sola: device + token + mappa normalizzazione + soglie di default
-- + istruzioni di collegamento per il protocollo. Catalogo PRODOTTO (globale), estendibile.

CREATE TABLE IF NOT EXISTS app.iot_blueprints (
    code          text PRIMARY KEY,            -- es. 'cold-chain'
    name          text NOT NULL,               -- nome del lavoro (IT)
    job           text NOT NULL DEFAULT '',    -- descrizione del lavoro
    category      text NOT NULL DEFAULT 'generico',
    protocol      text NOT NULL DEFAULT 'http', -- http | mqtt | lorawan
    key_map       jsonb NOT NULL DEFAULT '[]'::jsonb,       -- [{raw_key,metric,scale?,offset?}]
    default_rules jsonb NOT NULL DEFAULT '[]'::jsonb,       -- [{metric,op,threshold,etype,label}]
    icon          text NOT NULL DEFAULT 'sensor',
    sort          int  NOT NULL DEFAULT 100
);
GRANT SELECT ON app.iot_blueprints TO app_rt;

INSERT INTO app.iot_blueprints(code,name,job,category,protocol,key_map,default_rules,icon,sort) VALUES
 ('cold-chain','Catena del freddo','Monitora la temperatura di celle/frigoriferi e avvisa se sale troppo.','ambiente','http',
   '[{"raw_key":"t","metric":"temperature"},{"raw_key":"temp","metric":"temperature"},{"raw_key":"temperature","metric":"temperature"}]'::jsonb,
   '[{"metric":"temperature","op":">=","threshold":8,"etype":"sensore_critico","label":"Temperatura cella troppo alta"}]'::jsonb,'thermometer',10),
 ('tank-level','Livello serbatoio','Monitora il livello di un serbatoio/cisterna e avvisa quando scende sotto soglia.','processo','http',
   '[{"raw_key":"level","metric":"level"},{"raw_key":"lvl","metric":"level"},{"raw_key":"dist","metric":"distance"}]'::jsonb,
   '[{"metric":"level","op":"<=","threshold":10,"etype":"sensore_critico","label":"Livello serbatoio basso"}]'::jsonb,'gauge',20),
 ('energy-meter','Consumo energia','Rileva potenza ed energia di un quadro/macchinario per contabilizzare i consumi.','energia','http',
   '[{"raw_key":"power","metric":"power"},{"raw_key":"w","metric":"power"},{"raw_key":"energy","metric":"energy"},{"raw_key":"kwh","metric":"energy"}]'::jsonb,
   '[]'::jsonb,'bolt',30),
 ('air-quality','Qualità dell''aria','Monitora CO₂, polveri, temperatura e umidità di un ambiente interno.','ambiente','mqtt',
   '[{"raw_key":"co2","metric":"co2"},{"raw_key":"pm25","metric":"pm25"},{"raw_key":"t","metric":"temperature"},{"raw_key":"temperature","metric":"temperature"},{"raw_key":"hum","metric":"humidity"},{"raw_key":"humidity","metric":"humidity"}]'::jsonb,
   '[{"metric":"co2","op":">=","threshold":1000,"etype":"sensore_critico","label":"CO₂ elevata (areare)"}]'::jsonb,'wind',40),
 ('door-contact','Varchi e porte','Sorveglia l''apertura/chiusura di porte, cancelli e contatti.','stato','http',
   '[{"raw_key":"door","metric":"door"},{"raw_key":"contact","metric":"door"},{"raw_key":"open","metric":"door"}]'::jsonb,
   '[]'::jsonb,'door',50),
 ('people-count','Conteggio','Conta ingressi/presenze o impulsi da un contatore.','processo','http',
   '[{"raw_key":"count","metric":"count"},{"raw_key":"people","metric":"count"},{"raw_key":"n","metric":"count"}]'::jsonb,
   '[]'::jsonb,'hash',60),
 ('env-lorawan','Ambiente (LoRaWAN)','Sensore ambientale a lungo raggio/batteria via LoRaWAN (temperatura, umidità, batteria).','ambiente','lorawan',
   '[{"raw_key":"temperature","metric":"temperature"},{"raw_key":"humidity","metric":"humidity"},{"raw_key":"battery","metric":"battery"},{"raw_key":"batteryVoltage","metric":"battery"}]'::jsonb,
   '[{"metric":"battery","op":"<=","threshold":15,"etype":"batteria_critica","label":"Batteria sensore scarica"}]'::jsonb,'leaf',70)
ON CONFLICT (code) DO UPDATE SET name=EXCLUDED.name, job=EXCLUDED.job, category=EXCLUDED.category,
  protocol=EXCLUDED.protocol, key_map=EXCLUDED.key_map, default_rules=EXCLUDED.default_rules, icon=EXCLUDED.icon, sort=EXCLUDED.sort;
