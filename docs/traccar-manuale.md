# Tracciamento GPS (Traccar) — Manuale

Guida per l'amministratore YouWorkforce. Come **configurare** e **usare** il tracciamento.
Non serve mai aprire la console Traccar: tutto si fa da YouWorkforce.

Traccar è il motore GPS integrato: riceve le posizioni dei badge/dispositivi, rileva eventi
(movimento, soste, geofence, allarmi) e li trasforma in incidenti YouWorkforce. La catena di
escalation resta a GoAlert (vedi `docs/goalert-manuale.md`).

---

## Parte 1 — Configurazione (una volta)

### 1.1 Connessione (superadmin)
**Superadmin → tenant → Connessioni & integrazioni → Traccar**: base URL + credenziali. Di
norma è auto-provisionata; il semaforo segnala anomalie. Il modulo *traccar* deve essere attivo.

### 1.2 Impostazioni di sicurezza (admin → Connettori → Traccar → ⚙ Impostazioni sicurezza)
- **Cadenza poll**: ogni quanto YouWorkforce interroga Traccar (backfill eventi persi).
- **Soglia silenzio badge → perdita_contatto**: dopo quanti secondi un badge muto diventa
  allarme (rileva il silenzio anche se Traccar non manda *deviceOffline*).
- **Soglia down → traccar_conn_down**: quando Traccar irraggiungibile diventa allarme.

---

## Parte 2 — Uso

### 2.1 Dispositivi
**Dispositivi**: crea il device con **sorgente = traccar** e **ID esterno = uniqueId** del
badge (deve combaciare con Traccar). Assegna sede e lavoratore. Le posizioni appaiono in Mappa.

### 2.2 Comandi al device (Dispositivi → device traccar → Comandi)
Localizza (richiesta posizione), e — secondo il protocollo del device — stop/riavvio motore,
arma/disarma, riavvio. Lo storico mostra l'esito. Offline: i comandi si accodano e partono alla
riconnessione.

### 2.3 Report tracciamento (Dispositivi → device traccar → Report)
- **Viaggi**: tragitti con distanza e orari.
- **Soste**: fermate con durata.
- **Riepilogo**: distanza totale, velocità media/max del periodo.
- **CSV viaggi**: esporta.
I report coprono gli ultimi 7 giorni.

### 2.4 Contatori (odometro / ore)
Mostra odometro e ore motore del device; **azzera** quando serve (es. dopo manutenzione).

### 2.5 Traccar avanzato (Connettori → Traccar → 🛠 Traccar avanzato)

**Policy eventi** — per ogni tipo di evento Traccar decidi il trattamento:
- **ignora**: non registrato (rumore).
- **registra**: salvato come evento, non critico.
- **critico**: genera **allarme + escalation** (come un SOS).

Default sicuri per il lavoratore isolato:
| Evento | Default |
|---|---|
| Movimento non accompagnato | **critico** |
| Dispositivo inattivo | **critico** |
| Prossimità ingresso/uscita | registra |
| Eccesso velocità | registra |
| Calo carburante | registra |
| Manutenzione richiesta | registra |
| Cambio conducente | registra |
| Accensione/spegnimento motore | ignora |
| In movimento, online, esito comando | ignora |

> Gli eventi già gestiti (SOS, uomo a terra, manomissione, perdita alimentazione, jamming,
> batteria, offline, immobilità, ingresso/uscita sito) mantengono la loro logica dedicata.

**Conducenti (drivers)** — registra chi guida (nome + ID RFID/iButton) e collega a un device.
Quando il badge/veicolo riporta il conducente, l'evento *Cambio conducente* lo segnala.

**Manutenzioni** — pianifica una scadenza (per distanza, ore motore o data) su un device.
Al superamento, Traccar genera *Manutenzione richiesta* (trattato secondo la policy eventi).

**Attributi calcolati (computed)** — deriva un attributo dai dati grezzi del device con
un'espressione (JEXL). Esempi:
- Ignition da voltaggio: `power ? power > 13.2 : null` (tipo bool)
- Carburante da ingresso analogico: `(adc1 ?? null) != null ? adc1 * 0.065 : null` (num)
Collega l'attributo a un device: vale per tutte le sue posizioni.

---

## Parte 3 — Cosa succede in automatico
- **Pull + forwarding**: YouWorkforce riceve le posizioni/eventi via webhook e, in parallelo,
  fa polling per recuperare eventi persi (backfill) e rilevare i badge silenti.
- **Eventi → incidenti**: ogni evento è mappato secondo la **policy eventi**; i critici aprono
  un incidente e partono le notifiche (GoAlert).
- **Presenza**: calcolata da YouWorkforce confrontando le posizioni con le geofence (Mappa).
- **Batteria**: la policy batteria (off/warning/critico) decide la gravità.

---

## Parte 4 — Risoluzione problemi

| Sintomo | Causa | Rimedio |
|---|---|---|
| "Connessione Traccar non configurata" | manca base URL/credenziali | Superadmin → Connessioni |
| Device non riceve comandi | sorgente ≠ traccar o uniqueId errato | correggi il device |
| Report vuoti | nessuna posizione nel periodo | verifica che il device invii posizioni |
| Troppi eventi rumorosi | policy troppo permissiva | metti su *ignora* in Policy eventi |
| Evento importante non allarma | policy su *registra* | mettilo su *critico* |
| Attributo calcolato non compare | espressione errata o device non collegato | correggi l'espressione, collega il device |

---

## Note
- **Notifiche**: la consegna (SMS/voce/Slack) passa da GoAlert, non dai notificator Traccar.
- **Geofence**: si gestiscono in YouWorkforce (Mappa), non in Traccar.
- **Filtri posizione / geocoding**: configurati lato server dal superadmin.
