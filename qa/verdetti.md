# Tre verdetti indipendenti

## DEMO — **RAGGIUNTO** ✅
La catena critica è riproducibile su una macchina pulita e i simulatori sono dichiarati:
- SOS (badge simulato **e** PWA) → incidente → outbox → GoAlert SIMULATO → presa in carico → chiusura, con ID di correlazione end-to-end.
- Isolamento multitenant applicato in database (RLS), non solo in UI.
- Moduli attivabili/disattivabili per tenant senza deploy; disattivazione bloccata se toglie un percorso essenziale.
- Check-in con timer durevole che scade indipendentemente dai dati del badge.

Evidenza: `qa/smoke.sh`.

## PILOTA CONTROLLATO — **NON RAGGIUNTO**
Mancano, come richiesto dal perimetro:
- GoAlert **reale** con due livelli, canali controllati e rilettura delle prese in carico.
- Sorgente badge **reale** (Traccar + firmware), con prova di recupero eventi.
- Procedura operatori concordata e prova sul sito.
Condizioni per partire: vedi `offerta-pilota.md`.

## PRODUZIONE — **NON RAGGIUNTO**
Richiede inoltre: parità/sostituzione concordata delle funzioni legacy (Prompt 10),
piano di migrazione e rollback verificato, evidenza sul sito, supporto/licenze,
collaudo dei guasti, autorizzazione al passaggio operativo.

> Le 48 ore non attestano parità né autorizzano la sostituzione in produzione.
> Un local ack di GoAlert SIMULATO **non** prova la consegna di una notifica reale.
