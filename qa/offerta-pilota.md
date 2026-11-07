# Scheda — Pilota controllato (bozza)

## Problema risolto
Protezione dei lavoratori isolati con catena di allarme tracciabile (SOS → centrale →
reperibilità → chiusura con esito), su piattaforma modulare multitenant che unifica anche
forza lavoro, IoT e consuntivi — un solo prodotto, attivabile per capacità e per cliente.

## Funzioni dimostrate (DEMO, evidenza `qa/smoke.sh`)
- Onboarding tenant e moduli senza deploy; isolamento multitenant in database.
- Catena SOS completa con correlazione; check-in durevole; centrale live.
- Modularità per profili (Sicurezza dimostrato end-to-end).

## Integrazioni sperimentali / da verificare sul sito
GoAlert reale (2 livelli, canali), badge Traccar + firmware, ThingsBoard, YouWorker.ai (RAG/STT),
OpenDuty, Blues. Nessuna è provata in questa build.

## Prerequisiti sito/hardware per avviare il pilota
1. Istanza GoAlert autorizzata + recapiti di prova controllati (no numeri d'emergenza reali).
2. Badge/gateway con modello e firmware documentati; campione di eventi originali.
3. Staging con DNS `{slug}.loneworker.eu` + TLS; MFA superadmin.
4. Procedura di soccorso concordata e responsabilità di chi risponde.
5. Contratto YouWorker.ai (se si vuole RAG/voce) con separazione delle basi documentali.

## Evidenze da ottenere sul sito (vendita operativa)
Scenari di caduta/immobilità, copertura, falsi allarmi, chiamate e mancata risposta,
ripristino, governance dati, requisiti e licenze del mercato. Collaudo guasti.

## Criteri di fine pilota
Gate critici del perimetro in PASS con dispositivi e canali previsti; operatori con procedura;
il nuovo sistema **non** sostituisce le misure in uso finché non autorizzato.

> Nessuna certificazione o SLA dichiarati senza verifica. Vedi licenze in `qa/licenses.md` (da generare in CI).
