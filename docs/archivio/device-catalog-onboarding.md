# Device Catalog & Onboarding — analisi e progetto

> STATO: **Fase A+B+C implementate** (catalogo+ricerca, wizard provisioning, asset+assegnazione,
> attivo-al-primo-fix). Prova: `qa/device-onboarding-e2e.sh` (11/11). Restano D (cruscotto flotta
> avanzato), E (registrazione Traccar: base già nel wizard), F (analytics dedicate).
>
> CATALOGO: **importato l'elenco completo da traccar.org/devices** — ~2183 modelli, ~497 produttori
> (migration `0024`, modello→protocollo→porta). I 16 modelli lone-worker (migration `0022`) restano
> curati con **categoria** e **capacità** (SOS/caduta/battito…); gli importati hanno
> `category='altro'` e `capabilities=['gps']`, arricchibili aggiungendo righe/aggiornando il seed.


> Feature: catalogo dispositivi ricercabile → **wizard di onboarding che auto-configura** →
> cruscotto flotta + analytics → associazione semplice **Tenant → dispositivo → utente/asset**.
> Documento di progetto (nessun codice ancora). Autore: team YouWorkforce.

---

## 1. Obiettivo (cosa ho capito)

Oggi associare un dispositivo è macchinoso: l'admin deve sapere `external_id`, `source`,
protocollo, porta, e configurare a mano alarm_map, geofence, ecc. Vogliamo trasformarlo in un
**processo guidato**:

1. **Scelgo il dispositivo da un catalogo** ricercabile (come la lista
   [traccar.org/devices](https://www.traccar.org/devices/)), filtrabile per **produttore** e **scopo**
   (tracker personale, veicolo, asset, pet, sensore…).
2. Alla selezione, il sistema **auto-configura tutto il necessario**: protocollo, porta, endpoint di
   ingest, mappatura allarmi, capacità del device (track, batteria, SOS, caduta, battito…),
   e — per device reali — lo **registra anche su Traccar**.
3. Un **cruscotto flotta** mostra stato (online/offline), batteria, ultimo fix, assegnatario,
   capacità, e **analisi dei dati** rilevati.
4. **Assegno** il device a un **utente/operaio** oppure a un **asset/mezzo** e lo vedo **subito attivo**.

Catena da rendere banale: **Tenant → dispositivo → (utente | asset)**.

---

## 2. Stato attuale (e perché è macchinoso)

Abbiamo già (lato Traccar, vedi `README`):
- `devices` (external_id, source, label, site, assigned_user, enabled, config jsonb, ultima
  posizione/batteria/attributi), CRUD manuale.
- Ingest eventi + posizioni, geofence+presenza, comandi, report, filtri posizione.
- Config per-tenant `traccar/alarm_map`.

Cosa manca / attriti:
- **Nessun catalogo**: l'admin digita a mano `external_id` e `source`; non sa protocollo/porta.
- **Nessuna nozione di "modello"**: non sappiamo se un device ha SOS/caduta/battito → l'UI è generica.
- **Nessun concetto di asset/mezzo**: si può assegnare solo a un utente.
- **Nessun wizard**: la configurazione (alarm_map, ingest token, registrazione su Traccar) è separata.
- **Nessun cruscotto flotta** dedicato né analytics per device.
- **Auto-config assente**: niente pre-compilazione di protocollo/porta/capacità.

---

## 3. Modello dati proposto

### 3.1 Catalogo modelli (globale, curato)
```
device_models (
  model_code    text PK,          -- es. "teltonika-fmb965"
  manufacturer  text,             -- "Teltonika"  (derivato dal nome + curato)
  model_name    text,             -- "FMB965"
  protocol      text,             -- "teltonika"  (da lista Traccar)
  default_port  int,              -- 5027
  category      text,             -- personale | veicolo | asset | pet | sensore | gateway
  capabilities  text[],           -- ['gps','battery','sos','fall','heart_rate','steps',...]
  osmand        boolean,          -- supporta protocollo OsmAnd (smartphone/app)
  default_config jsonb,           -- preset alarm_map + parametri suggeriti
  notes         text,
  recommended   boolean
)
```
Globale, sola lettura per i tenant (come `modules_catalog`).

### 3.2 Capacità (enum applicativo)
`gps, battery, charge, sos, fall, man_down, heart_rate, steps, temperature, humidity,
two_way_audio, geofence, motion, ignition, immobilizer`.
Guidano l'UI (quali pannelli/allarmi mostrare) e i default di `alarm_map`.

### 3.3 Asset (nuovo) — per assegnare a mezzi/cose, non solo persone
```
assets (id, tenant_id, name, kind,           -- veicolo | attrezzatura | varco | altro
        identifier, site_id, note, created_at)
```
`devices.assigned_asset uuid REFERENCES assets(id)` (oltre a `assigned_user`): un device è
assegnato a **una persona OPPURE un asset** (vincolo: non entrambi).

### 3.4 Device arricchito
`devices` aggiunge: `model_code` (FK catalogo), `assigned_asset`.
Da `model_code` deriviamo protocollo, porta, capacità → niente più campi a mano.

### 3.5 Provisioning / stato onboarding
```
device_provisioning (device_id PK, step text,         -- creato|configurato|traccar_registrato|in_attesa_primo_fix|attivo
                     traccar_device_id int, last_error text, updated_at)
```
Traccia l'avanzamento del wizard e lo stato "attivo" (primo fix ricevuto).

---

## 4. Esperienza target — Wizard di onboarding (4 passi)

**Passo 1 — Scegli il modello**
- Campo di ricerca (come Traccar) + filtri **Produttore** e **Scopo** (categoria).
- Risultato: card con produttore, modello, scopo, **badge capacità** (SOS, caduta, batteria…),
  "RECOMMENDED" dove previsto. "Non in lista? → Generico/OsmAnd (smartphone)".

**Passo 2 — Identifica il dispositivo**
- Inserisci `IMEI`/`uniqueId` (il sistema spiega dove trovarlo per quel modello).
- Etichetta + sede (opz.).

**Passo 3 — Assegna**
- A **utente** (operaio) **oppure** **asset** (mezzo/attrezzatura). Ricerca con autocomplete.
- (Opz.) sede, gruppo allarme di riferimento.

**Passo 4 — Attiva (auto-config)**
Il sistema fa **in automatico**:
1. crea il `device` con `model_code` → capacità e `source` dedotti;
2. imposta `alarm_map`/parametri dal `default_config` del modello (solo se il tenant non ha override);
3. per device reale: **registra il device su Traccar** (API) con protocollo/uniqueId;
4. mostra all'installatore le **istruzioni di collegamento**: `host:porta` (protocollo del modello) o,
   per smartphone, il link OsmAnd + token;
5. stato → *in attesa primo fix* → quando arriva la prima posizione → **attivo** (badge verde).

Risultato: in 4 schermate il device è **associato e attivo**.

---

## 5. Cruscotto flotta + analytics

**Flotta (tabella/griglia)** con ricerca e filtri (produttore, scopo, stato, sede, assegnatario):
- stato **online/offline** (da ultimo fix + soglia), **batteria** (barra), **ultimo contatto**,
  **assegnato a** (utente o asset), **capacità** (icone), modello.
- azioni rapide: localizza, apri mappa, storico, riassegna, disabilita.

**Analytics per device / flotta**:
- batteria nel tempo (trend), frequenza fix (salute del tracking),
- allarmi per tipo/periodo (SOS, caduta, batteria, offline),
- tempo dentro/fuori geofence (presenza), km percorsi, soste.
- export CSV (riusa i report #5).

---

## 6. Il catalogo: da dove vengono i dati

La lista [traccar.org/devices](https://www.traccar.org/devices/) fornisce **solo**
`modello (con produttore nel nome) → protocollo → porta`. **Non** ha categoria né capacità.
Quindi:

- **Import automatico** (protocollo+porta) via scraping/one-off della pagina Traccar → tabella
  `device_models` (manutenibile, versionata nel repo come seed).
- **Curazione** di `manufacturer` (split dal nome), `category` e `capabilities` **da parte nostra**:
  partiamo da un **sottoinsieme rilevante lone-worker** (tracker personali con SOS/caduta/battito:
  es. Teltonika TMT250, Concox, Queclink, smartphone/OsmAnd) e ampliamo.
- **Fallback "Generico/OsmAnd"**: sempre disponibile per smartphone/app o device non catalogati.

> Onesto: le capacità per 1500+ modelli non sono nel dato Traccar. Curiamo i modelli che servono
> davvero (decine), il resto resta "generico" finché non serve.

---

## 7. Cosa significa "auto-configura tutto"

Dal `model_code` ricaviamo e applichiamo:
- **source / protocollo / porta** → istruzioni di collegamento corrette.
- **capabilities** → l'UI mostra solo i pannelli pertinenti (battito solo se wearable, ecc.) e i
  **default alarm_map** sensati (es. tracker personale: SOS critico, caduta=man_down, batteria 15%).
- **registrazione su Traccar** (se connessione tenant configurata, vedi #2 comandi): POST device su
  Traccar con `uniqueId`+nome → il forwarding già instrada eventi/posizioni verso di noi.
- **ingest**: usa il token di ingest del tenant esistente (nessun passo manuale).

---

## 8. Associazione semplificata (il cuore della richiesta)

Oggi: Tenant (da token) → device (a mano) → utente (a mano, campi separati).
Dopo: **un wizard unico** che lega in un flusso:

```
Tenant ─▶ [catalogo: scegli modello] ─▶ [IMEI + etichetta]
        ─▶ [assegna a Utente | Asset] ─▶ [Attiva] ─▶ attivo
```
- Riassegnazione in 1 click dal cruscotto (device → altro utente/asset), con storico.
- Un device può stare su **utente** o **asset**; un asset può avere più device (es. mezzo con
  tracker + sensore temperatura).

---

## 9. API & migrazioni (proposta)

**Migrazioni**
- `00xx_device_catalog.sql`: `device_models` (+ seed curato) .
- `00xx_assets.sql`: `assets`; `devices.model_code`, `devices.assigned_asset`; vincolo user XOR asset.
- `00xx_provisioning.sql`: `device_provisioning`.

**Endpoint**
- `GET /api/catalog/devices?q=&manufacturer=&category=` → ricerca catalogo (tenant).
- `GET /api/catalog/facets` → elenco produttori/categorie per i filtri.
- `POST /api/devices/provision` → crea+configura+(registra Traccar)+assegna in un colpo (body del wizard).
- `GET /api/fleet` → cruscotto (device + stato + batteria + assegnatario + capacità, con filtri).
- `GET /api/fleet/analytics?device=&from=&to=` → serie batteria/fix/allarmi (riusa report).
- `CRUD /api/assets` → gestione asset.
- `POST /api/devices/:id/assign` → riassegna a utente/asset.

**Auth/RBAC**: provisioning e catalogo = `tenant_admin`; cruscotto = operatore/admin/auditor;
RLS per tenant come il resto.

---

## 10. UI (viste console)

- **Dispositivi → "+ Aggiungi (wizard)"**: 4 step (catalogo → identifica → assegna → attiva).
- **Flotta**: griglia filtrabile + stato/batteria/capacità + azioni rapide.
- **Dettaglio device**: tab *Stato* (telemetria live), *Mappa/Percorso*, *Analytics*, *Assegnazione*.
- **Asset**: anagrafica mezzi/attrezzature + device collegati.
- Catalogo: ricerca tipo Traccar con chip produttore/scopo e badge capacità.

---

## 11. Integrazione Traccar reale

- Se il tenant ha la **connessione Traccar** (già fatta, #2 comandi), il provisioning **crea il device
  su Traccar** via API (`POST /api/devices`), così il forwarding funziona senza passi manuali.
- Istruzioni installatore generate dal modello: `protocollo @ host:porta` o OsmAnd per smartphone.
- Senza connessione Traccar: resta la modalità "ingest diretto" (OsmAnd/token) già esistente.

---

## 12. Fasi di rilascio (stima)

| Fase | Contenuto | Stima |
|---|---|---|
| **A** | `device_models` + seed curato (20-40 modelli rilevanti) + ricerca/filtri catalogo (API+UI) | media |
| **B** | Wizard provisioning (crea+config+assegna) + stato "attivo al primo fix" | media |
| **C** | Asset (CRUD) + assegnazione utente/asset + riassegnazione da cruscotto | piccola/media |
| **D** | Cruscotto flotta (stato/batteria/capacità + filtri) | media |
| **E** | Registrazione automatica su Traccar nel wizard | piccola/media |
| **F** | Analytics device/flotta (batteria/fix/allarmi) + export | media |

Ordine consigliato: **A → B → C → D → E → F**. A+B+C già danno l'esperienza "semplice" richiesta.

---

## 13. Rischi / limiti onesti

- **Capacità per modello**: dato non fornito da Traccar → curazione manuale di un sottoinsieme; il
  resto "generico". Va bene: copriamo i device che servono.
- **Registrazione Traccar**: richiede connessione+credenziali per-tenant (vault in produzione).
- **Catalogo 1500+ modelli**: importiamo protocollo/porta per tutti, ma arricchiamo (categoria/
  capacità) solo i rilevanti; manteniamo il seed nel repo.
- **"Attivo subito"**: dipende dal primo fix reale del device → mostriamo *in attesa primo fix* finché
  non arriva (onesto, non fittizio).

---

## 14. Criteri di accettazione (test)

- Ricerca catalogo per testo/produttore/scopo restituisce i modelli attesi.
- Wizard: da modello scelto → device creato con capacità+protocollo corretti, assegnato, e (con
  Traccar) registrato; stato passa a **attivo** al primo fix simulato.
- Assegnazione a **utente** e ad **asset** (XOR) + riassegnazione con storico.
- Cruscotto: filtri produttore/scopo/stato; batteria/ultimo contatto corretti; isolamento per tenant.
- Analytics: serie batteria/fix/allarmi su intervallo + export CSV.
- RBAC: provisioning solo tenant_admin; cruscotto per operatore/auditor; isolamento RLS.

---

## Conclusione

La richiesta è centrata e fattibile **riusando quasi tutto** ciò che esiste (ingest, posizioni,
geofence, comandi, report). Il nuovo valore è: **catalogo curato + wizard + asset + cruscotto**, che
rendono banale la catena **Tenant → dispositivo → utente/asset** e danno visibilità/analisi sulla
flotta. Partendo da **Fase A+B+C** si ottiene subito l'onboarding semplice richiesto.
