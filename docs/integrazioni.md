# Integrazioni: gestionale (ERP), software turni, agente Youco

Fase 2 del piano «Idealverde su YouWorkforce». **YW non chiama sistemi esterni: sono loro a chiamare
YW**, con una chiave di integrazione. È il punto d'aggancio del connettore MCP di Youco verso l'ERP:
Youco legge le commesse dal gestionale e le scrive in YW con queste API; riprende da YW l'esito degli
interventi e i consuntivi e li riporta al gestionale. Migrazione `0166`, codice
`core/src/integrations.rs`, test `qa/integrations-e2e.sh`, pagina della console **Configurazione →
Integrazioni**.

## Chiavi di integrazione

- Le crea chi ha il permesso «Impostazioni» (`settings.manage`), solo dalla console (una chiave non
  gestisce altre chiavi).
- Una chiave **agisce per conto di chi la crea**, con i permessi scelti tra quelli che quella persona ha
  (mai `admin`). Se la persona perde un permesso, lo perde anche la chiave; se viene eliminata o
  anonimizzata, la chiave smette di funzionare. Funziona solo finché l'azienda non è sospesa.
- Si mostra **una volta sola** (`ywk_<prefisso>_<segreto>`); in DB c'è solo l'impronta SHA-256. Si
  revoca dalla console (effetto immediato) e può scadere (1–730 giorni, 0 = mai).
- Uso: intestazione `Authorization: Bearer ywk_…`, senza cookie e senza token CSRF.
- Le azioni compaiono nel registro attività come «persona · chiave «nome»».

Permessi ammessi: `plan` (clienti, commesse, interventi), `orders.view_all`, `timesheets.view_all`,
`approve`, `costs.view`, `sites.manage`, `users.manage`, `config.view`.

Con una chiave si possono usare anche le altre API della console nei limiti dei permessi scelti
(es. `GET /api/orders?state=open`, `POST /api/orders/check`, `GET /api/activity-types`).

## Codice esterno

Clienti, sedi, commesse, interventi, persone, mezzi, tipi di attività e gruppi hanno `external_ref`,
il codice del gestionale, unico per azienda. Le chiamate `PUT` per codice sono **idempotenti**:
ripetute non creano doppioni, aggiornano. I campi non passati restano com'erano.

| Metodo | Percorso | Permesso | Corpo |
|---|---|---|---|
| PUT | `/api/integration/clients/{ref}` | `plan` | `name`, `vat`, `address`, `phone`, `email`, `pec`, `sdi`, `payment_terms`, `notes` |
| PUT | `/api/integration/sites/{ref}` | `sites.manage` | `name`, `address`, `lat`, `lon`, `access_notes`, `access_windows` |
| PUT | `/api/integration/commesse/{ref}` | `plan` (modulo Cantieri) | `name`, `code` (default = ref), `client_ref`, `site_ref`, `start_date`, `end_date` (AAAA-MM-GG), `budget_amount`, `status` (pianificato, in_corso, sospeso, chiuso), `note` |
| PUT | `/api/integration/orders/{ref}` | `plan` (modulo Interventi) | `title`, `site_ref`, `client_ref`, `activity_type` e `team` (codice esterno o nome), `assignee_email`, `quantity`, `scheduled_start`, `scheduled_end` (RFC 3339), `priority` (1–3), `instructions` |

Risposta: `{"id", "ref", "created": true|false}` (per gli interventi anche `warnings`). Un codice di
riferimento sconosciuto (`client_ref`, `site_ref`, …) → 400 con il motivo.

**Interventi.** Passano dal **controllo dei vincoli** come quelli creati in console
([lavoro-campo-fase1.md](lavoro-campo-fase1.md)): un vincolo duro (fuori orario della sede, patentino
mancante, persona assente, mezzo fermo o già prenotato) → 409 «Conflitto: …»; con `x-force: 1` si
procede comunque. Senza fine indicata la fine si calcola dalla durata del tipo di attività. Un
intervento completato o annullato non si modifica (409).

## Elenco delle modifiche (verso il gestionale)

`GET /api/integration/changes?since=<RFC 3339>&limit=500` (permesso `orders.view_all` o `plan`):

- `orders`: interventi cambiati dopo `since`, con stato, esito, inizio e fine, arrivo, minuti lavorati,
  firmatario, `ref`, `site_ref`, `client_ref`, tipo di attività, squadra, assegnatario;
- `timesheets` (con `timesheets.view_all` o `approve` e modulo Consuntivi): consuntivi cambiati, con
  stato, intervento collegato (`order_ref`), persona e minuti.

`until` è il valore da usare come prossimo `since`. Se `more` è vero ci sono altre modifiche: si
richiama subito. Le modifiche si leggono a un istante del database, quindi nessuna cade tra due
chiamate.

## Importazione CSV

`POST /api/integration/import/{clients|sites|commesse|orders}`, corpo `text/csv` (anche dalla console:
**Integrazioni → Importa da file CSV**). Prima riga con i nomi delle colonne (gli stessi campi della
tabella sopra), colonna obbligatoria `ref`; separatore `;` o `,`, campi tra virgolette ammessi, fino a
5.000 righe. Ogni riga si inserisce o aggiorna da sola: le righe con errori sono nel rapporto
`errors: [{row, ref, error}]` e non fermano le altre. Risposta: `rows`, `created`, `updated`,
`with_warnings`, `errors`.

## Meteo (fase 4)

Con il modulo **Meteo operativo** attivo, chi porta le previsioni (l'agente Youco) usa una chiave con
il permesso *Pianificare*: `PUT /api/integration/weather/zones/{codice}` `{name, lat, lon}` per le
zone e `POST /api/integration/weather/slots` `{zone, source, issued_at, slots:[{from, to, rain_mm,
rain_prob, wind_kmh, gust_kmh, temp_c, condition}]}` per le fasce orarie (fino a 2000 per invio; una
nuova emissione sostituisce le fasce con lo stesso inizio). Esempio:

```
curl -X POST https://<tenant>/api/integration/weather/slots -H "Authorization: Bearer ywk_…"   -H "content-type: application/json" -d '{"zone":"MI-CN","source":"Youco","issued_at":"2026-11-26T06:00:00Z",
  "slots":[{"from":"2026-12-02T07:00:00Z","to":"2026-12-02T10:00:00Z","rain_mm":4.5,"rain_prob":90,
  "wind_kmh":22,"gust_kmh":48,"temp_c":6,"condition":"pioggia"}]}'
```

Dettaglio, limiti e avvisi in [meteo-operativo.md](meteo-operativo.md).

## Pianificazione (fase 5)

Con il modulo **Pianificazione automatica** un motore esterno (Youco) legge il problema con
`GET /api/planning/problem?from&to` (contratto `yw-planning/1`: squadre, persone con patentini e
assenze, interventi con posizione, finestre, durata, requisiti e giorni di meteo sfavorevole, mezzi con
prenotazioni). Consegna la proposta con `POST /api/planning/runs/external`. YW la valida con i propri
vincoli e la tiene come bozza da rivedere e applicare in console. Dettaglio in
[pianificazione-automatica.md](pianificazione-automatica.md).
