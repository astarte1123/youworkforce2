-- Connessione alla piattaforma AI di YouCo (youworker.ai), gestita DA YouWorkforce
-- (superadmin), come per Traccar/ThingsBoard: base URL + API key (cifrata a riposo).
-- Provider intercambiabile: contratto OpenAI-compatibile parametrico (path/model
-- configurabili). Se non configurata, i processi AI degradano al piano deterministico
-- locale senza rompere l'inserimento manuale, il check-in o l'SOS.
CREATE TABLE IF NOT EXISTS app.ai_connections (
    tenant_id  uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    base_url   text NOT NULL,
    api_key    text NOT NULL,                                   -- enc:v1:... (cifrata)
    model      text NOT NULL DEFAULT 'youworker-1',
    chat_path  text NOT NULL DEFAULT '/v1/chat/completions',    -- endpoint chat
    stt_path   text NOT NULL DEFAULT '/v1/audio/transcriptions',-- endpoint speech-to-text (diario vocale)
    updated_at timestamptz NOT NULL DEFAULT now()
);
