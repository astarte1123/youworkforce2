# Collaudo staging — badge GPS, perdita di contatto, escalation (mig 0127/0128)

Scheda per chi collauda su staging e per il responsabile operativo che prepara il rilascio.
Riferimenti: [dispositivi-gps.md](dispositivi-gps.md) (funzionamento), [escalation-manuale.md](escalation-manuale.md).

Tutti i controlli SQL citati sono **in sola lettura** (`BEGIN TRANSACTION READ ONLY … ROLLBACK`) e
si eseguono con un ruolo che vede tutti i tenant:

```sh
psql -U postgres -d youworkforce -v ON_ERROR_STOP=1 -f qa/sql/<controllo>.sql
```

---

## 1. Prima del deploy: controlli per livello di migrazione

Livello dello schema:

```sql
SELECT max(version) FROM app._sqlx_migrations;
```

| Livello già applicato | Controlli da eseguire prima del deploy | Cosa guardare |
|---|---|---|
| **< 125** (nomi `traccar`, GoAlert attivo) | `check-escalation-0127.sql`, `check-imei-contesi.sql`, `check-badge-0128.sql` | tutti e tre: l'upgrade applica 0125 → 0128 in sequenza |
| **125 o 126** | `check-escalation-0127.sql`, `check-imei-contesi.sql`, `check-badge-0128.sql` | come sopra |
| **127** | `check-imei-contesi.sql`, `check-badge-0128.sql` | l'escalation è già migrata: il suo controllo non è più significativo |
| **128** | `check-imei-contesi.sql`, `check-badge-0128.sql` | stato reale dopo l'upgrade |

### 1a. Incidenti che possono generare un nuovo avviso (solo prima della 0127)

`check-escalation-0127.sql` elenca ogni incidente aperto con l'esito previsto:

| Esito | Effetto all'avvio dei worker | Azione prima del deploy |
|---|---|---|
| `chiusura` | incidente "server di tracciamento irraggiungibile" chiuso, escalation ferma | nessuna |
| `invariato_preso_in_carico` | non toccato | nessuna |
| `continuita` | prosegue dal gradino registrato, nessun doppione | nessuna |
| `riavvio_primo_gradino` | **nuovo avviso** dal primo gradino (finora nessuno era stato avvisato davvero) | avvisare i reperibili che riceveranno un avviso per un incidente già aperto |
| `possibile_doppio_avviso` | **il primo gradino può arrivare due volte** (già inviato da GoAlert, ora dal nativo) | prendere in carico o chiudere prima del deploy se già gestito, altrimenti avvisare i reperibili |

Gli incidenti `perdita_contatto` esistenti **restano aperti** (nota nel controllo): l'origine non è
verificabile, li chiude un operatore.

### 1b. IMEI contesi

`check-imei-contesi.sql`: un IMEI presente in più tenant è **rifiutato dal bridge** (log
`identità ambigua`), quindi **nessun tenant riceve dati né SOS** da quel badge. Il riepilogo finale
riporta `imei_contesi` e `con_lavoratori_monitorati`.

Azione: con il responsabile operativo di ciascun tenant decidere il titolare, eliminare il device
doppione nel tenant sbagliato, verificare il primo contatto nel tenant giusto. Se
`con_lavoratori_monitorati > 0` è un **blocco al rilascio**: quei lavoratori resterebbero senza
protezione.

### 1c. Badge che resteranno in attesa dopo la 0128

`check-badge-0128.sql` mostra per ogni badge lo stato dopo la 0128 (prima della 0128 lo calcola con
la stessa regola della migrazione) e la colonna **`azione_responsabile`**:

| Situazione | Stato dopo 0128 | Azione richiesta al responsabile operativo |
|---|---|---|
| IMEI conteso | — | decidere il tenant titolare, eliminare il doppione, verificare il primo contatto |
| assegnato, **mai collegato** | `in_attesa` | **lavoratore NON protetto**: accendere il badge, verificare il primo contatto nella scheda, poi *Metti in servizio* |
| non assegnato | `in_attesa` | alla consegna: assegnare al lavoratore, poi *Metti in servizio* |
| disabilitato ma assegnato | `fuori_servizio` | se in uso: riabilitare e *Metti in servizio*; altrimenti togliere l'assegnazione |
| disabilitato | `fuori_servizio` | nessuna |
| in servizio ma silente da oltre un giorno | `in_servizio` | dopo l'upgrade scatta *perdita di contatto* alla soglia: verificare il badge o metterlo *fuori servizio* |
| in servizio, contatto recente | `in_servizio` | nessuna (monitoraggio conservato) |

Consegnare l'elenco filtrato per tenant al rispettivo responsabile **prima** del deploy: dopo
l'upgrade i badge `in_attesa` non generano alcun allarme di silenzio.

---

## 2. Collaudo con il GH-5200 su staging

Prerequisiti:
- tenant di collaudo con modulo **gps** attivo, lavoratore di prova assegnato al GH-5200, routing
  allarmi `sos` / `man_down` verso un gruppo con **solo il recapito di test** (telefono/email di
  prova, mai reali);
- soglia di silenzio (*Collegamento dispositivi → Soglie e allarmi*) ridotta per il collaudo, ad es.
  **180 s**, da ripristinare a fine prova;
- periodo di invio del badge (anche in sleep) **inferiore alla soglia**: un badge fermo che invia
  ogni 15 minuti con soglia 10 minuti genera falsi allarmi;
- core avviato **senza** `TEST_CLOCK` (orologio reale; `/api/_test/*` deve rispondere 403).

| # | Prova | Come | Esito atteso |
|---|---|---|---|
| 1 | **Primo contatto** | accendere il GH-5200 puntato a staging, porta 5027 | bridge accetta l'IMEI (nessun log `identità ambigua`); nella scheda compaiono ultimo contatto e posizione; stato **ancora `in_attesa`**; **nessun incidente** |
| 2 | **Messa in servizio** | scheda dispositivo → *Metti in servizio* | stato `in_servizio`; riga di audit; da qui parte il conteggio del silenzio |
| 3 | **SOS** | pulsante SOS (IO 236) | incidente critico `sos` nel solo tenant di collaudo, con posizione del lavoratore; escalation al primo gradino |
| 4 | **Man-down** (se configurato sul badge) | caduta simulata / inclinazione (IO 242) | incidente critico `man_down`, stessa escalation del SOS; se il rilevamento caduta non è attivo nella configurazione del badge la prova è *non applicabile*, non *superata* |
| 5 | **Consegna al recapito di test** | pulsante *prova* accanto al recapito del reperibile, poi le prove 3/4 | il recapito riceve **una** notifica per gradino e canale; timeline incidente con `esc_delivery` reale; nessun doppione; la presa in carico ferma i gradini successivi |
| 6 | **Perdita di contatto** | spegnere il badge (o schermarlo) con badge in servizio | dopo soglia + ≤ 10 s: **un** incidente critico `perdita_contatto` con età di ultimo contatto e ultima posizione; escalation al recapito di test |
| 7 | **Ritorno online** | riaccendere il badge | ultimo contatto aggiornato, badge di nuovo online; l'incidente della prova 6 **resta aperto** finché un operatore non lo prende in carico e lo chiude; **nessun nuovo incidente** finché non c'è un nuovo silenzio oltre soglia |
| 8 | **Fuori servizio** | scheda → *Fuori servizio*, poi spegnere il badge | badge visibile con stato `fuori_servizio`; **nessun** incidente oltre soglia |
| 9 | **Isolamento** | stesso IMEI registrato in un secondo tenant di staging | registrazione rifiutata (409); SOS e posizioni restano nel tenant titolare |

A fine collaudo: ripristinare la soglia, chiudere gli incidenti di prova, lasciare il badge nello
stato voluto (in servizio solo se protegge davvero qualcuno).

### Dopo il deploy, sorveglianza log
- `non più del tenant`: connessioni chiuse per cambio titolarità (attese solo dopo spostamenti voluti);
- `identità ambigua`: IMEI conteso non risolto (deve essere zero);
- raffiche di `perdita_contatto` subito dopo l'avvio: badge in servizio silenti da tempo (vedi 1c).

---

## 3. Cosa è già coperto in locale e cosa no

Coperto in locale con connessioni e notifiche simulate (`qa/gps-isolation-e2e.sh`,
`qa/gps-service-watchdog-e2e.sh`, `qa/upgrade-0127-check.sh`, `qa/upgrade-0128-check.sh`,
`qa/gh5200-lifesafety-e2e.sh`): decodifica IO 236/242, isolamento, comandi, watchdog con orologio
controllato, upgrade e transizione escalation.

Richiedono il **badge fisico**: prove 1, 3, 4, 6, 7 (radio, GPS reale, periodo di invio in sleep,
rilevamento caduta del firmware).
Richiedono **accesso a staging/produzione**: tutta la sezione 1, le prove 2, 5, 8, 9, la consegna
reale sul canale (SMS/voce/email) e la sorveglianza log.
