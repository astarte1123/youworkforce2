-- Per far avvisare davvero i reperibili, GoAlert ha bisogno di un recapito per
-- ogni utente (contact method SMS/voce). Il numero del lavoratore deve poter essere
-- inserito DA YouWorkforce (anagrafiche), non nella UI di GoAlert. Formato E.164.
ALTER TABLE app.users ADD COLUMN IF NOT EXISTS phone text;
