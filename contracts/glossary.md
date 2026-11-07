# Glossario (oggetti comuni tra moduli)

- **Tenant**: cliente isolato. Identità stabile `uuid`, slug di dominio.
- **Sede / cantiere** (`site`): organizza il luogo.
- **Commessa**: il lavoro da rendicontare (backlog Prompt 9).
- **Ordine di lavoro**: unità assegnata; contesto per istruzioni, RAG, ore, materiali (backlog Prompt 9).
- **Squadra / gruppo**, **persona** (`user`), **dispositivo** (`device`).
- **Evento normalizzato**: vedi `events.schema.json`.
- **Incidente**: `aperto → preso_in_carico → intervento → chiuso(esito)`. Autorità: Rust.
- **Sessione di protezione** / **check-in**: timer durevole nel core, indipendente dai dati del badge.
- **Turno pianificato / presenza / ordine / riga consuntivo**: registri **distinti**; l'avvio di uno non certifica gli altri.
- **Outbox**: richiesta notifica persistita nella stessa transazione dell'incidente; drenata dal worker verso GoAlert (at-least-once).
- **Profilo evidenza**: `SIMULATO | UPSTREAM REALE | HARDWARE REALE | SITO PILOTA`.

## Autorità singola per regola
- ThingsBoard rileva soglie sensori → Rust **crea e governa** incidenti → GoAlert esegue la reperibilità.
- Perdita motore/canale ⇒ **degrado** visibile, mai un secondo motore di escalation.
