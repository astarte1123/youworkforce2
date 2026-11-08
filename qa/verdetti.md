# Tre verdetti indipendenti

## DEMO — **RAGGIUNTO** ✅
La catena critica è riproducibile su una macchina pulita e i simulatori sono dichiarati:
- SOS (badge simulato **e** PWA) → incidente → outbox → GoAlert SIMULATO → presa in carico → chiusura, con ID di correlazione end-to-end.
- Isolamento multitenant applicato in database (RLS), non solo in UI.
- Moduli attivabili/disattivabili per tenant senza deploy; disattivazione bloccata se toglie un percorso essenziale.
- Check-in con timer durevole che scade indipendentemente dai dati del badge.

Evidenza: `qa/smoke.sh`.

## PILOTA CONTROLLATO — **NON RAGGIUNTO** (ma GoAlert e Traccar ora reali)
Fatto in questa build:
- **GoAlert v0.35.0 reale** integrato: il core crea allarmi reali sul servizio con policy di escalation a **2 livelli** (oncall1→oncall2) e correlazione tracciata. Evidenza `qa/goalert-e2e.sh` (6/6). Routing per-tenant via chiave di integrazione.
- **Traccar v6.17.0 reale** integrato come sorgente badge: device (protocollo OsmAnd) → Traccar → *event forwarding* → adapter core `/api/ingress/traccar` → stessa catena SOS normalizzata → incidente → GoAlert reale. Due upstream reali in catena. Mapping allarmi `sos→sos`, `fallDown→man_down`. Tenant dal token di ingest (mai dal body). Evidenza `qa/traccar-e2e.sh` (8/8).
- **ThingsBoard v3.8.1 reale** integrato come sorgente IoT: telemetria device → *Rule Engine* reale (nodo REST API Call aggiunto alla Root Rule Chain) → adapter core `/api/ingress/thingsboard` → gate di severità (soglia → `sensore_critico`) → incidente → GoAlert reale. Tre upstream reali. Evidenza `qa/thingsboard-e2e.sh` (8/8). ThingsBoard CE è BUSL: istanza esterna.

Mancano ancora per il perimetro:
- **Consegna reale** delle notifiche (SMS/voce/webhook) e verifica/contact-method del 2° reperibile: cablare il trasporto di test per sito.
- **Firmware badge hardware reale** (il server Traccar è reale; manca il dispositivo fisico sul campo e la distinzione perdita-rete vera).
- Procedura operatori concordata e prova sul sito.
Condizioni per partire: vedi `offerta-pilota.md`.

## PRODUZIONE — **NON RAGGIUNTO**
Richiede inoltre: parità/sostituzione concordata delle funzioni legacy (Prompt 10),
piano di migrazione e rollback verificato, evidenza sul sito, supporto/licenze,
collaudo dei guasti, autorizzazione al passaggio operativo.

> Le 48 ore non attestano parità né autorizzano la sostituzione in produzione.
> Un local ack di GoAlert SIMULATO **non** prova la consegna di una notifica reale.
