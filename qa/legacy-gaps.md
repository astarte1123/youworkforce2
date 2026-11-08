# Prompt 10 — Parità YouWorkforce, migrazione e passaggio clienti

**Stato complessivo: NON VERIFICATO.** Nessun accesso al YouWorkforce esistente in questa
sessione. **La parità NON è passata per assenza di dati.** Questo documento consegna: (a) la
matrice con le lacune, (b) l'elenco preciso degli input necessari, (c) il disegno di migrazione,
(d) un **dry-run su fixture dichiarata** (`qa/legacy-dryrun.py`) che prova il *meccanismo*
idempotente di import/riconciliazione — **non** la migrazione di un cliente reale.

> Nota di metodo: le righe sotto NON inventano funzioni legacy. Elencano le **capacità del
> nuovo prodotto** e, per ciascuna, l'**evidenza legacy** richiesta per confermare la parità.
> Finché l'evidenza manca, lo stato resta `non_verificata`.

## Input necessari per completare la matrice (bloccanti)
1. Accesso in sola lettura autorizzato al YouWorkforce esistente **oppure** export + manuali.
2. Elenco **ruoli effettivi** e permessi reali in uso (non quelli teorici).
3. **API/export** disponibili: formato, schema, volumi, chiavi/identificativi storici.
4. **Integrazioni** in uso: badge/GPS, centrale allarmi, ERP/presenze/paghe, canali notifica (SMS/voce/email).
5. Per cliente: anagrafiche, gruppi, sedi, assegnazioni, dispositivi, configurazioni, **storico** disponibile.
6. Regole di business non ovvie: tariffe, arrotondamenti, SLA, calendari, turni, festività.
7. Vincoli di passaggio: finestre, responsabilità, criteri di rollback, periodo di doppio esercizio.

## Matrice di parità
Colonne: `funzione/entità | chi la usa (da confermare) | evidenza legacy richiesta | mappatura nel nuovo prodotto | possibile comportamento differente | stato`

| Funzione/entità | Chi la usa | Evidenza legacy richiesta | Mappatura nel nuovo prodotto | Comportamento differente (ipotesi) | Stato |
|---|---|---|---|---|---|
| Anagrafica utenti/ruoli | tutti | export utenti + matrice ruoli/permessi reale | `users` (ruoli: tenant_admin, operatore, pianificatore, responsabile, lavoratore, auditor) | set ruoli potrebbe non combaciare 1:1 | non_verificata |
| Sedi/cantieri | pianificazione, campo | export sedi (id, nome, geo) | `sites` | geocodifica/gerarchie sedi | non_verificata |
| Dispositivi badge/GPS | sicurezza, IoT | inventario device + protocolli | `devices` (source traccar) + ingress Traccar reale | identificativi/protocolli diversi | non_verificata |
| Assegnazioni/ordini | pianificazione, campo | export ordini + stati | `work_orders` (bozza→assegnato→in_corso→completato→annullato) | nomenclatura stati/transizioni | non_verificata |
| Turni | pianificazione | definizione turni/calendari | `shifts` (inizio/fine) | ottimizzatore turni non previsto (H48) | non_verificata |
| Consuntivi ore/spese | campo, amministrazione | export righe + regole tariffarie | `timesheets`+`timesheet_lines`+`expenses` (centesimi, CSV) | arrotondamenti/valute/regole costo | non_verificata |
| Allarmi/escalation | centrale | policy escalation + reperibilità | GoAlert reale (EP multi-livello, consegna webhook verificata) | motore unico GoAlert; mapping policy | da_decidere |
| IoT/telemetria | tecnico | sorgenti/soglie | ThingsBoard reale → `sensore_critico` | soglie/allarmi da ricalibrare | non_verificata |
| Diario/note di lavoro | campo | formato note/allegati | `diary_entries` (voce browser/manuale) | allegati/foto = backlog | non_verificata |
| Storico/audit | amministrazione, auditor | formato log + retention | `audit_log` + timeline incidenti | granularità/retention diverse | non_verificata |
| Documenti/allegati | campo, amministrazione | tipi/volumi/storage | **non implementato** (backlog) | — | da_sostituire |
| Integrazione ERP/paghe | amministrazione | API/formati | contratto import/export (backlog) | — | da_decidere |

Stati ammessi: `verificata | da_migrare | da_sostituire | da_decidere | non_verificata`.
Nessuna riga è `verificata`: nessuna evidenza legacy disponibile.

## Disegno di migrazione (per tenant, staging, dry-run)
- **Idempotente**: upsert per chiave esterna storica (`legacy_id`); un re-run non duplica.
- **Staging per tenant**: import in un tenant di staging, riconciliazione conteggi/riferimenti, poi promozione.
- **Riconciliazione**: conteggi attesi vs importati; riferimenti risolti (es. device→sede); **record scartati con motivazione** (orfani, chiavi mancanti, formati invalidi).
- **Ordine**: anagrafiche → sedi → dispositivi → assegnazioni → configurazioni → storico (ove disponibile).
- **Canale allarme separato dai dati**: un solo sistema operativo per sito, finestra e responsabilità di passaggio, **rollback verificato**; doppio esercizio finché non collaudato.
- **Divieti**: non disattivare il legacy né passare dispositivi reali senza accordo e collaudo sul sito.

## Dry-run su fixture dichiarata
`python qa/legacy-dryrun.py qa/fixtures/legacy_sample.json` — valida e riconcilia una **fixture
dichiarata** (non dati cliente), **senza scrivere su DB**: normalizza, deduplica per `legacy_id`,
rileva orfani/formati invalidi, e stampa un report (da_migrare / scartati con motivazione).
Questo prova il **meccanismo**; **non** dimostra la migrazione di alcun cliente.
