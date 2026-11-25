-- SL-15 + SA-10 (decisioni PO 26/11/2026): due cataloghi di prodotto nel DB, gestiti dal superadmin.
--
-- 1) app.event_types: UNICO catalogo dei tipi di evento (nome leggibile, famiglia, spiegazione).
--    Prima le etichette stavano in Rust e nella console, diverse tra loro, e «dispositivo offline»
--    e «area» avevano quattro forme ciascuna. Ora ogni codice ha una FAMIGLIA (connessione, area,
--    soccorso, …) che raggruppa le varianti. Il superadmin modifica nome, famiglia e spiegazione.
--    NON è nel DB (e non è modificabile) se un tipo è critico o salva-vita: lo decide il prodotto
--    (api.rs CRITICAL / LIFE_SAFETY), così un allarme salva-vita non può essere declassato.
-- 2) app.plans_catalog: i pacchetti proposti alla creazione di un tenant (prima fissi nel codice).
--    I predefiniti si modificano o disattivano ma non si eliminano.
CREATE TABLE IF NOT EXISTS app.event_types (
    code        text PRIMARY KEY CHECK (code ~ '^[a-z][a-z0-9_]{1,60}$'),
    label       text NOT NULL CHECK (length(btrim(label)) BETWEEN 2 AND 120),
    category    text NOT NULL CHECK (category IN ('soccorso','connessione','area','dispositivo','veicolo','sensori','ronde','altro')),
    description text NOT NULL DEFAULT '',
    sort        int  NOT NULL DEFAULT 100,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text
);
GRANT SELECT ON app.event_types TO app_rt;

INSERT INTO app.event_types(code,label,category,description,sort) VALUES
 ('sos','SOS / richiesta soccorso','soccorso','Il lavoratore ha premuto il pulsante di soccorso.',10),
 ('man_down','Uomo a terra (man down)','soccorso','Il dispositivo ha rilevato una caduta o una posizione a terra.',11),
 ('immobilita','Immobilità prolungata','soccorso','Nessun movimento per più del tempo previsto.',12),
 ('mancato_checkin','Check-in mancato','soccorso','Il lavoratore non ha confermato di stare bene entro la scadenza.',13),
 ('perdita_contatto','Perdita contatto (badge silente)','connessione','Il dispositivo di un lavoratore protetto non comunica più: trattato come allarme.',20),
 ('device_offline','Dispositivo offline','connessione','Il dispositivo non comunica (non è un allarme).',21),
 ('heartbeat_down','Segnale periodico assente','connessione','Un sistema che deve segnalarsi a intervalli regolari ha smesso di farlo.',22),
 ('dispositivo_inattivo','Dispositivo inattivo','connessione','Il dispositivo non invia dati da tempo.',23),
 ('dispositivo_inattivo_critico','Dispositivo inattivo (critico)','connessione','Dispositivo inattivo, gestito come allarme.',24),
 ('traccar_conn_down','Collegamento di localizzazione interrotto (storico)','connessione','Evento del vecchio collegamento esterno, non più in uso.',29),
 ('geofence_ingresso','Ingresso in zona vietata','area','Il lavoratore è entrato in un’area vietata.',30),
 ('geofence_uscita','Uscita da area sicura','area','Il lavoratore è uscito da un’area in cui deve restare.',31),
 ('geofence','Fuori area','area','Posizione fuori dall’area prevista.',32),
 ('ingresso_sito','Ingresso in sito','area','Ingresso nell’area di una sede.',33),
 ('uscita_sito','Uscita dal sito','area','Uscita dall’area di una sede.',34),
 ('presenza_ingresso','Ingresso in area','area','Ingresso registrato in un’area (presenza).',35),
 ('presenza_uscita','Uscita dall’area','area','Uscita registrata da un’area (presenza).',36),
 ('attraversamento_geofence','Attraversamento area','area','Il dispositivo ha attraversato il confine di un’area.',37),
 ('prossimita_ingresso','Avvicinamento','area','Due dispositivi si sono avvicinati.',38),
 ('prossimita_uscita','Allontanamento','area','Due dispositivi si sono allontanati.',39),
 ('prossimita_ingresso_critico','Avvicinamento (critico)','area','Avvicinamento gestito come allarme.',40),
 ('prossimita_uscita_critico','Allontanamento (critico)','area','Allontanamento gestito come allarme.',41),
 ('manomissione_badge','Manomissione / rimozione badge','dispositivo','Il badge è stato tolto o manomesso.',50),
 ('perdita_alimentazione','Perdita alimentazione','dispositivo','Il dispositivo ha perso l’alimentazione esterna.',51),
 ('jamming','Disturbo del segnale','dispositivo','Il segnale di localizzazione o rete è disturbato.',52),
 ('batteria_critica','Batteria in esaurimento (critico)','dispositivo','Batteria quasi scarica: il dispositivo sta per spegnersi.',53),
 ('batteria_bassa','Batteria bassa','dispositivo','Batteria sotto la soglia impostata.',54),
 ('device_alarm','Allarme dispositivo','dispositivo','Allarme inviato dal dispositivo senza un tipo riconosciuto.',55),
 ('device_event','Evento dispositivo','dispositivo','Evento generico del dispositivo.',56),
 ('device_moving','In movimento','dispositivo','Il dispositivo si sta spostando.',57),
 ('esito_comando','Esito comando','dispositivo','Risposta del dispositivo a un comando inviato.',58),
 ('media_ricevuto','Foto o audio ricevuto','dispositivo','Il dispositivo ha inviato una foto o un audio.',59),
 ('movimento_non_accompagnato','Movimento non accompagnato','veicolo','Il mezzo si muove senza il conducente previsto.',60),
 ('movimento_non_accompagnato_critico','Movimento non accompagnato (critico)','veicolo','Movimento non accompagnato, gestito come allarme.',61),
 ('eccesso_velocita','Eccesso di velocità','veicolo','Superato il limite di velocità impostato.',62),
 ('eccesso_velocita_critico','Eccesso di velocità (critico)','veicolo','Eccesso di velocità gestito come allarme.',63),
 ('calo_carburante','Calo carburante','veicolo','Calo improvviso del livello di carburante.',64),
 ('aumento_carburante','Aumento carburante','veicolo','Rifornimento rilevato.',65),
 ('accensione_motore','Accensione motore','veicolo','Il motore è stato acceso.',66),
 ('spegnimento_motore','Spegnimento motore','veicolo','Il motore è stato spento.',67),
 ('manutenzione_richiesta','Manutenzione richiesta','veicolo','Il mezzo ha raggiunto la scadenza di manutenzione.',68),
 ('manutenzione_richiesta_critico','Manutenzione richiesta (critico)','veicolo','Manutenzione scaduta, gestita come allarme.',69),
 ('cambio_conducente','Cambio conducente','veicolo','È cambiato il conducente del mezzo.',70),
 ('sensore_critico','Sensore critico','sensori','Un sensore ha superato una soglia di allarme.',80),
 ('allarme_sensore','Allarme sensore','sensori','Allarme segnalato da un sensore.',81),
 ('telemetria','Telemetria (nessun allarme)','sensori','Dati registrati senza allarme.',82),
 ('email_alarm','Allarme ricevuto via email','altro','Allarme arrivato da un sistema esterno tramite email.',90),
 ('ronda_mancata','Ronda mancata','ronde','Un giro di ronda pianificato non è stato svolto in orario.',85)
ON CONFLICT (code) DO NOTHING;

CREATE TABLE IF NOT EXISTS app.plans_catalog (
    code        text PRIMARY KEY CHECK (code ~ '^[a-z][a-z0-9_-]{1,40}$'),
    name        text NOT NULL CHECK (length(btrim(name)) BETWEEN 2 AND 80),
    description text NOT NULL DEFAULT '',
    modules     text[] NOT NULL DEFAULT '{}',
    active      boolean NOT NULL DEFAULT true,
    builtin     boolean NOT NULL DEFAULT false,
    sort        int NOT NULL DEFAULT 100,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text
);
GRANT SELECT ON app.plans_catalog TO app_rt;
INSERT INTO app.plans_catalog(code,name,description,modules,builtin,sort) VALUES
 ('sicurezza','Sicurezza lavoratore','SOS, uomo a terra, reperibilità.','{loneworker}',true,10),
 ('campo','Lavoro sul campo','Interventi, cantieri, ronde, consuntivi, turni.','{interventi,cantieri,giri,consuntivi,workforce}',true,20),
 ('iot','IoT','Sensori, telemetria, soglie che aprono allarmi, cruscotti.','{iot}',true,30),
 ('full','Completo','Tutti i moduli e l’assistente AI.','{loneworker,gps,interventi,cantieri,giri,consuntivi,workforce,iot,assistant}',true,40)
ON CONFLICT (code) DO NOTHING;
