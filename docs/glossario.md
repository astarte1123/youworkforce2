# Glossario di YouWorkforce

Un nome per ogni cosa, uguale in console, app del lavoratore, messaggi e manuali (decisione PO
AR-7 del 26/11/2026). I nomi delle API e delle tabelle **non cambiano**: la colonna «Nel codice»
serve a chi sviluppa per ritrovare la cosa.

Il controllo `qa/glossary-check.py` (in `qa/run-all.sh`) fallisce se nei testi della console o del
manuale utente ricompare un nome vecchio.

## Gruppi

| Si dice | Cos'è | Nel codice | Non dire |
|---|---|---|---|
| **Gruppo di persone** | insieme di persone dell'anagrafica (squadra, reparto, zona…), riusabile da tutti i moduli | `user_groups`, `/api/user-groups` | gruppo utenti |
| **Squadra** | gruppo di persone di tipo «squadra» | `user_groups.kind='squadra'` | — |
| **Gruppo di escalation** | chi riceve gli allarmi, con rotazione di reperibilità e policy dei passi | `alarm_groups`, `/api/groups` | gruppo allarme, gruppo di allarme, gruppo di reperibili |

Un gruppo di escalation può prendere i membri da uno o più gruppi di persone (sempre allineati).

## Turni

| Si dice | Cos'è | Nel codice |
|---|---|---|
| **Turno pianificato** | turno di lavoro messo in calendario dal pianificatore (modulo Forza lavoro e turni) | `shifts` (stato pianificato), `shift_plans` |
| **Turno in servizio** | il lavoratore ha aperto il turno nell'app («Inizio turno» / «Fine turno»): durante il turno in servizio il silenzio del telefono e del badge è sorvegliato | `shifts` con `state='in_corso'` (aperto dall'app o avviato) |
| **Turno di reperibilità** | periodo in cui una persona di un gruppo di escalation è il reperibile di turno | rotazioni dei gruppi, `oncall` |

Da evitare: «turno on-call» (si dice turno di reperibilità), «override» (si dice sostituzione).

## Ronde

| Si dice | Cos'è | Nel codice | Non dire |
|---|---|---|---|
| **Ronde** (modulo «Ronde e vigilanza») | il modulo delle ronde con punti di controllo | modulo `giri` | giri |
| **Percorso di ronda** | sequenza ordinata di punti di controllo, con sede, durata massima e orari | `patrol_routes`, `/api/patrol-routes` | giro |
| **Ronda** | un passaggio di una guardia su un percorso, con le sue punzonature | `patrol_rounds` | giro |
| **Punto di controllo** (checkpoint) | QR, tag NFC, beacon o punto GPS da punzonare | `patrol_checkpoints` | — |
| **Ronda mancata** | un orario pianificato in cui nessuno ha avviato la ronda | etype `ronda_mancata` | — |

## Eventi e allarmi

I nomi dei tipi di evento stanno nel **catalogo dei tipi di evento** (superadmin → Cataloghi):
un nome e una famiglia per ogni codice, uguali ovunque. Vedi `docs/manuale.md` § 13-quater.

| Si dice | Cos'è |
|---|---|
| **Evento** | qualcosa che il sistema registra (ingresso in area, batteria bassa…) |
| **Allarme** | evento che apre un incidente in Centrale e avvia l'escalation |
| **Allarme salva-vita** | SOS, uomo a terra, immobilità, check-in mancato, perdita contatto: non si perde mai e non si declassa |
| **Presa in carico** | un operatore o un reperibile conferma di occuparsene (ferma l'escalation) |
