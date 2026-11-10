# Progetto: Generatore multiagentico di scenari (da descrizione + immagini)

Genera scenari YouWorkforce **ricchi** a partire dai casi d'uso reali di
[thingsboard.io/use-cases](https://thingsboard.io/use-cases/): per ciascun caso, una
**pipeline multiagentica** legge la pagina (descrizione + composizione delle dashboard mostrate
nelle immagini) e produce una **spec di scenario** (JSON) che il motore YouWorkforce usa per
cruscotto, telemetria demo e analisi AI. Uno alla volta, con verifica umana.

## Vincolo (copyright)
Le **immagini/branding/screenshot** del sito sono protetti: gli agenti li **analizzano** per
capire quali widget/metriche mostra una dashboard reale, ma **non si copia nessun asset**. Gli
output sono **ricostruzioni originali** (palette YouCo, dati del tenant).

## Pipeline (per scenario)
1. **Research agent** (web-fetch): legge `/use-cases/<slug>/`. Estrae, come dati strutturati:
   summary, device/asset, metriche (nome, unità, range demo realistico), widget della dashboard
   (tipo + metrica + soglie), KPI, allarmi, presenza mappa.
2. **Spec agent**: mappa l'estratto allo schema YouWorkforce → `scenario_specs/<code>.json`
   (metriche con range/widget, narrativa, kpi, mappa). Allinea le **regole** del catalogo
   (`scenario_catalog`) alle soglie reali (es. voltage>253 / <207, frequency<49.5).
3. **Generator**: la spec è additiva e caricata a runtime (`SCENARIO_SPECS_DIR`, default
   `scenario_specs/`). Nessun deploy: il cruscotto e la telemetria demo la usano se presente.
4. **Verify agent / umano**: attiva lo scenario + demo su un tenant e controlla cruscotto,
   incidenti, analisi AI. Se ok → prossimo scenario.

Orchestrazione: per i 16 scenari la pipeline è fan-out (un ramo per scenario, research→spec→
verify in parallelo). Si esegue **uno alla volta** finché lo schema è validato, poi in blocco.

## Schema spec (`scenario_specs/<code>.json`)
```json
{
  "code": "smart_energy",
  "name": "Smart Energy",
  "source": "<url use-case>",
  "narrative": "<descrizione sintetica ricostruita>",
  "map": false,
  "metrics": [
    { "key": "voltage", "label": "Tensione", "unit": "V",
      "min": 200, "max": 260, "normal": [228,232], "alarm": 255, "widget": "gauge" }
  ],
  "kpis": ["device_count","alarms_now","open_incidents","events_24h"]
}
```
- `metrics[].widget`: `gauge` (barra+soglia+trend) · `line` (trend marcato) · `stat` (KPI numerico) · `status` (booleano).
- `normal`: intervallo per la telemetria demo normale; `alarm`: valore che fa scattare la regola.
- Le **soglie d'allarme** restano guidate dalle **regole** del catalogo (coerenza con la catena incidenti).

## Integrazione runtime (già implementata)
- `scenario_spec(code)` carica la spec (opzionale, additiva).
- `scenario_widgets_spec(scn, spec)`: widget dalla spec se presente, altrimenti derivati dalle regole.
- `scenario_dash_json`: aggiunge `narrative` + `generated:true` + widget ricchi.
- `super_scenario_demo`: se c'è la spec, genera **12 campioni normali variati** (serie/grafici
  realistici) + 1 allarme per regola critica; altrimenti il comportamento storico.
- Analisi AI per-scenario (`/scenarios/:code/ai-analysis`) fa grounding su questi dati.

## Stato
- **smart_energy**: generato e verificabile (spec + catalogo allineato a tensione/corrente/
  frequenza/energia, contatori A/B/C). Primo da validare insieme.
- Restanti 15: da generare con la stessa pipeline dopo l'ok.

## Test
`qa/scenarios-e2e.sh` copre apply/demo/cruscotto/KPI/mappa/analisi AI. Per uno scenario con
spec si verifica in più: narrativa, widget estesi (metriche non-regola), serie multi-punto.
