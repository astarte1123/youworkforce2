# Progetto: Ambienti / Casi d'uso (ThingsBoard use-cases su YouWorkforce)

Replica dei casi d'uso reali di [thingsboard.io/use-cases](https://thingsboard.io/use-cases/)
come **scenari attivabili dal superadmin su qualsiasi tenant**, riusando le primitive di
YouWorkforce (moduli, regole sensori, device/sedi, catena incidenti, AI). Nessun ambiente
separato: ogni scenario è un **bundle di configurazione** applicato al tenant.

## Principio
Uno scenario NON è codice nuovo per use-case: è un **profilo parametrico** nel catalogo
(`scenario_catalog()` in `core/src/api.rs`) che, applicato, provisiona:
1. **Moduli** necessari (`tenant_modules.state='attivo'`).
2. **Bundle di regole sensori** (`app.sensor_rules`, label `scn:<code>:<metric>:<op>` → idempotente/rimovibile).
3. **(opz.) Demo**: un sito `"<nome> (demo)"` + device `scn-<code>-<suffix>` (`source='scenario'`).
4. **Stato** in `app.tenant_scenarios` (per attivare/rimuovere in modo pulito).

Gli allarmi usano gli **etype esistenti** (`sensore_critico`, `man_down`, `telemetria`, …):
gli etype critici entrano nella catena incidenti già pronta (incidente → GoAlert → routing →
turni) e sono leggibili/azionabili anche dall'**agente AI**. Pull e push ThingsBoard
convergono sullo stesso motore regole.

## Attivazione (superadmin, qualsiasi tenant)
- `GET /api/superadmin/scenarios` — catalogo (16).
- `GET /api/superadmin/tenants/:id/scenarios` — scenari applicati al tenant.
- `POST /api/superadmin/tenants/:id/scenarios` `{code, with_demo}` — applica.
- `DELETE /api/superadmin/tenants/:id/scenarios/:code` — rimuove (regole bundle + device demo
  non referenziati; i **moduli restano abilitati**).
- UI: card tenant → **"Ambienti (casi d'uso)"**, raggruppati per settore, Attiva/Rimuovi +
  spunta "crea demo".

## Catalogo (16 scenari → mappatura)
| Code | Nome | Settore | Moduli | Regole chiave (metric op soglia → etype) |
|---|---|---|---|---|
| `smart_energy` | Smart Energy | Energia | iot, thingsboard | power_kw≥50→crit · voltage≥250/≤200→crit · energy_kwh≥1000→tele |
| `environment_monitoring` | Environment Monitoring | Ambiente | iot | temperature≥35→crit · humidity≥85→tele · co2≥1500→crit |
| `smart_office` | Smart Office | Edifici | iot | co2≥1200→crit · temperature≥30→tele · smoke=true→crit |
| `water_metering` | Water Metering | Utility | iot, thingsboard | leak=true→crit · fraud=true→crit · flow≥100→tele |
| `smart_retail` | Smart Retail (catena freddo) | Retail | iot | temperature≥8→crit · temperature≤-25→crit · door_open=true→tele |
| `smart_farming` | Smart Farming | Agricoltura | iot | soil_moisture≤20→crit · temperature≤2→crit · humidity≥90→tele |
| `site_fleet_tracking` | Site Fleet Tracking | Mezzi | traccar, loneworker, iot | fuel≤10→crit · engine_temp≥110→crit (geofence via Traccar) |
| `smart_metering` | Smart Metering | Utility | iot, thingsboard | leak=true→crit · fraud=true→crit · consumption≥1000→tele |
| `health_care` | Health Care (assisted living) | Sanità | loneworker, iot, goalert | heart_rate≥120/≤40→crit · fall=true→man_down |
| `air_quality` | Air Quality Monitoring | Ambiente | iot | pm25≥35→crit · co2≥1000→crit · humidity≥90→tele · temperature≥40→tele |
| `smart_irrigation` | Smart Irrigation | Agricoltura | iot | soil_moisture≤15→crit · flow≤1→tele |
| `waste_management` | Waste Management | Utility | iot | fill_level≥90→crit · fill_level≥70→tele |
| `tank_level` | Tank Level Monitoring | Industria | iot, thingsboard | level≤10→crit · level≥95→crit (overflow) |
| `scada_pool` | SCADA Swimming Pool | SCADA | iot, thingsboard | ph≤6.8/≥7.8→crit · chlorine≤0.5→crit · temperature≥32→tele |
| `scada_oil_gas` | SCADA Oil & Gas Drilling | SCADA | iot, thingsboard, goalert | pressure≥350→crit · gas≥20→crit · vibration≥80→crit |
| `scada_energy` | SCADA Energy Management | SCADA | iot, thingsboard | demand_kw≥500→crit · power_factor≤0.8→tele |

`crit` = `sensore_critico`, `tele` = `telemetria`. Le soglie sono default ragionevoli,
**modificabili per tenant** dalla card Regole sensori (o dall'agente AI con conferma).

## Dati / migrazioni
- `0047_scenarios.sql`: `app.tenant_scenarios(tenant_id, scenario_code, with_demo, applied_by, applied_at)`.
- Riusa `app.sensor_rules`, `app.tenant_modules`, `app.sites`, `app.devices`.

## Reversibilità e sicurezza
- Rimozione cancella solo il bundle dello scenario (label `scn:<code>:%`) e i device demo non
  referenziati da eventi (FK-safe). I moduli restano (potrebbero servire ad altro).
- Idempotente: riapplicare non duplica le regole.
- Solo superadmin applica/rimuove (audit `scenario_apply`/`scenario_remove`).

## Telemetria demo (dimostrazione end-to-end)
`POST /api/superadmin/tenants/:id/scenarios/:code/demo` inietta, per i device demo dello
scenario, 1 payload tutto-normale (telemetria) + 1 payload d'allarme per ogni regola critica
(max 3). I valori sono derivati dalle soglie (`demo_alarm_value`/`demo_normal_value`) e passano
dallo **stesso percorso del push** (`eval_sensor_rules` → `persist_ingest`): gli allarmi
diventano **incidenti reali** in Centrale (con GoAlert/routing). UI: pulsante **⚡ Demo** accanto
allo scenario attivo. Non lascia regole/device nuovi: solo eventi/incidenti (correlation
`scndemo-<code>-…`).

## Cruscotti parametrici (dashboard in-app, ricostruiti da zero)
Ogni scenario deriva i **widget** dalle sue regole (`scenario_widgets`): una card per metrica
— **gauge** numerico (valore, barra, soglia, direzione, sparkline) o **stato** booleano — con
unità/etichetta per metrica. Non si copia la grafica del sito: si ricostruisce con la palette
YouCo e i **dati reali** del tenant.
- `GET /api/scenarios` — scenari attivi del tenant corrente.
- `GET /api/scenarios/:code/dashboard` — widget + ultimo valore + serie (ultimi 30 punti) +
  incidenti recenti, dai device demo dello scenario (RLS; solo se lo scenario è attivo).
- UI: voce menu **Cruscotti IoT** (operatore/admin/auditor, moduli iot/thingsboard), tab per
  scenario, gauge/sparkline SVG inline.

Tipi di widget: **gauge** (numerico), **status** (booleano), **KPI aggregati** (device, metriche,
allarmi ora, incidenti aperti, eventi 24h), **mappa device** (Leaflet, coord demo generate
all'attivazione), **multi-metrica** (sparkline sovrapposte normalizzate).

### Preset AI per-scenario
Ogni scenario ha un angolo d'analisi (`scenario_ai_focus`). `POST /api/scenarios/:code/ai-analysis`
`{save?}` fa grounding sui dati reali del cruscotto (widget+KPI+incidenti) e chiede al provider
AI un'analisi specifica dello scenario (stato/anomalie/azioni), opz. salvata come report AI
(`kind=<code>`). UI: pulsante **✦ Analisi AI** nel cruscotto. Richiede provider AI configurato
(superadmin) + modulo assistant.

## Estensione
Aggiungere uno scenario = una voce in `scenario_catalog()` (nessuna migrazione): regole, moduli,
device demo e widget si derivano da lì. Estensione residua: preset AI per-scenario (report/analisi tipici).

## Test
`qa/scenarios-e2e.sh` (18): catalogo 16, applica air_quality+demo (moduli/regole/sito/device/
stato), elenco tenant, idempotenza, **telemetria demo → incidenti reali**, rimozione pulita.
