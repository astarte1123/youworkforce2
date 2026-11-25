# Archivio — documenti di progetto storici

Questi documenti raccontano come YouWorkforce è stato progettato nelle fasi precedenti. Sono
conservati come memoria delle decisioni, ma **descrivono architetture superate**: in particolare
le integrazioni con server esterni (Traccar, GoAlert, ThingsBoard), rimosse dal prodotto con la
migrazione `0125_remove_external_connectors.sql`.

Per il funzionamento attuale fanno fede il [`README`](../../README.md) e i manuali in [`docs/`](..).

| Documento | Di cosa parlava |
|---|---|
| `porting-project.md` | piano di porting modulare (fase connettori) |
| `native-escalation-engine-project.md` | progetto del motore di escalation nativo (sostituzione di GoAlert) |
| `ywalert-parity-project.md` | parità funzionale del motore di escalation |
| `ywalert-manuale.md` | vecchio manuale di reperibilità con YWAlert come motore o rollback (ritirato) |
| `worker-safety-rationalization.md` | razionalizzazione del perimetro sicurezza lavoratore |
| `device-catalog-onboarding.md` | catalogo dispositivi e onboarding (prima della decodifica nativa) |
| `scenario-generation-project.md` | generatore di scenari IoT |
| `LoneWorker_Prompt_Orchestrazione_48h.md` | prompt di orchestrazione iniziale |
| `verdetti.md`, `acceptance-matrix.md`, `legacy-gaps.md`, `offerta-pilota.md` | verdetti e matrici di accettazione della fase demo |
| `accesso-demo.md` | accessi e credenziali della demo con i motori esterni |
