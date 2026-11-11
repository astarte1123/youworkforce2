# Tracciamento GPS (Traccar) — Manuale YouWorkforce

Guida completa per l'amministratore. Come **configurare** e **usare** il tracciamento GPS da
YouWorkforce. **Non serve mai aprire la console Traccar**: ogni cosa si fa dalle schermate YW.

Traccar è il motore GPS integrato: riceve le posizioni dei badge/dispositivi, rileva eventi
(movimento, soste, geofence, allarmi, inattività) e YouWorkforce li trasforma in incidenti e
notifiche. La **consegna** delle notifiche (SMS/voce/Slack) resta a GoAlert — vedi
`docs/goalert-manuale.md`.

**Chi fa cosa**
| Ruolo | Può |
|---|---|
| Superadmin | connessione Traccar, filtri server, attivazione modulo |
| Tenant admin | tutto il resto: device, policy eventi, drivers, manutenzioni, computed, report |
| Operatore | inviare comandi, vedere report |

---

## Parte 1 — Configurazione iniziale (una volta)

### 1.1 Attivare il motore (superadmin)
1. **Superadmin → tenant → Connessioni & integrazioni**.
2. Verifica che il modulo **traccar** sia *attivo* (semaforo verde). Di norma è auto-provisionato.
3. **Traccar — connessione**: `base URL` (es. `http://traccar.tuodominio:8082`), utente e password
   di un account Traccar con permessi. Il semaforo diventa verde se raggiungibile.

> Le credenziali sono scritte cifrate (write-only). Nessuno può rileggerle.

### 1.2 Impostazioni di sicurezza (tenant admin)
**Connettori → Traccar — salute connessione → ⚙ Impostazioni sicurezza**:
- **Cadenza poll** (5–600 s): ogni quanto YW interroga Traccar per recuperare eventi persi
  (backfill da cursore). Default 30 s.
- **Soglia silenzio badge → `perdita_contatto`** (s, 0=off): dopo quanti secondi un badge che
  non invia più nulla diventa allarme. Rileva il silenzio **anche se Traccar non manda
  `deviceOffline`**. Per il lavoratore isolato è un paracadute: imposta es. 600 s.
- **Soglia down → `traccar_conn_down`** (30–3600 s): quando l'intero Traccar è irraggiungibile
  diventa allarme (il sistema di tracciamento è giù).
- **Poll attivo**: on/off.

### 1.3 Forward fast-path (opzionale, bassa latenza)
Oltre al poll, Traccar può **spingere** gli eventi verso YW in tempo reale. In **Connettori →
Traccar → Verifica collegamento** trovi le env da impostare all'avvio del container Traccar
(`EVENT_FORWARD_URL`, `FORWARD_URL`, `FORWARD_ENABLE=true`) con un pulsante *copia env*. È
opzionale: il poll garantisce comunque la consegna.

---

## Parte 2 — Dispositivi

### 2.1 Registrare un device
**Dispositivi → + nuovo**:
- **Sorgente** = `traccar`.
- **ID esterno** = `uniqueId` del badge/tracker (deve combaciare **esattamente** con quello
  configurato su Traccar).
- **Etichetta**, **Sede**, **Assegnato a** (il lavoratore che porta il badge).
- **Abilitato**.

Da quel momento le posizioni appaiono in **Mappa** e la presenza nelle geofence è calcolata
automaticamente.

### 2.2 Comandi al device (Dispositivi → device traccar → Comandi)
Catalogo comandi (dipende dal protocollo del device):
| Comando | Effetto |
|---|---|
| 📡 Localizza (`positionSingle`) | richiesta posizione immediata |
| ⛔ Stop motore (`engineStop`) | blocca il motore |
| ▶ Riavvia motore (`engineResume`) | sblocca il motore |
| 🔒 Arma / 🔓 Disarma (`alarmArm`/`alarmDisarm`) | antifurto |
| ↻ Riavvia device (`rebootDevice`) | reboot del tracker |

I comandi disponibili sono rilevati dal protocollo. Se il device è **offline**, il comando si
accoda e parte alla riconnessione. Lo **storico comandi** mostra l'esito.

### 2.3 Report tracciamento (Dispositivi → device traccar → Report)
Ultimi 7 giorni:
- **🚗 Viaggi**: tragitti con inizio/fine e distanza.
- **⏸ Soste**: fermate con durata.
- **📊 Riepilogo**: distanza totale, velocità media e massima.
- **⬇ CSV viaggi**: esporta per foglio di calcolo.

### 2.4 Contatori (odometro / ore)
**mostra** legge odometro e ore motore dal device. **azzera** li riporta a zero (es. dopo una
sostituzione o manutenzione).

---

## Parte 3 — Traccar avanzato
**Connettori → Traccar — salute connessione → 🛠 Traccar avanzato**. Quattro sezioni.

### 3.1 Policy eventi — il controllo più importante
Decidi, per ogni tipo di evento che Traccar genera, cosa deve fare YouWorkforce:
- **ignora**: non registrato (rumore, non interessa).
- **registra**: salvato come evento, **non** critico (consultabile, nessuna escalation).
- **critico**: genera **allarme + escalation** (come un SOS: parte la reperibilità via GoAlert).

**Tabella eventi e default (safety-first per il lavoratore isolato):**
| Evento Traccar | Significato | Default |
|---|---|---|
| Movimento non accompagnato | il device si muove senza un altro device vicino | **critico** |
| Dispositivo inattivo | nessuna posizione da molto tempo (persona ferma/scomparsa) | **critico** |
| Prossimità ingresso | entra vicino a un device collegato | registra |
| Prossimità uscita | si allontana da un device collegato | registra |
| Eccesso velocità | sopra la soglia `speedLimit` | registra |
| Calo carburante | perdita rapida di carburante | registra |
| Aumento carburante | rifornimento | ignora |
| Accensione motore | ignition on | ignora |
| Spegnimento motore | ignition off | ignora |
| Manutenzione richiesta | superata una scadenza (vedi 3.3) | registra |
| Cambio conducente | cambia il `driverUniqueId` | registra |
| Media ricevuto | foto/video/audio dal device | ignora |
| In movimento | inizio movimento | ignora |
| Attraversamento geofence | crossing lungo il tragitto | registra |
| Esito comando | risposta a un comando | ignora |
| Online / Sconosciuto | stato connessione | ignora |

> **Gestiti a parte** (logica dedicata, non in questa tabella): SOS, uomo a terra, manomissione
> badge, perdita alimentazione, jamming, batteria, offline, immobilità, ingresso/uscita sito.
> Questi mantengono il loro comportamento.

**Come si usa:** cambia il menu a tendina accanto all'evento e premi **Salva policy**. Effetto
immediato sui nuovi eventi. Esempio: in cantiere metti *Eccesso velocità* = **critico**; in
flotta logistica lo lasci *registra*.

### 3.2 Conducenti (drivers)
Chi guida/opera un mezzo o porta un badge con identificazione.
1. **Nome** + **ID (RFID/iButton)** = il valore letto dal lettore (deve combaciare esatto).
2. (opz.) **device**: lo collega subito a un dispositivo.
3. **+ aggiungi**.
Quando il device riporta il conducente, l'evento **Cambio conducente** segnala chi è a bordo.
Se il device non manda il conducente ma ne è collegato uno solo, viene usato quello.

### 3.3 Manutenzioni
Scadenze automatiche su un device:
1. **Nome** (es. "Tagliando").
2. **Tipo**: *distanza* (km percorsi), *ore motore*, *data/tempo*.
3. **Inizio**: prima soglia (es. 10000 km).
4. **Periodo**: intervallo per le successive (es. 15000 km → scatta a 10k, 25k, 40k…).
5. **device** + **+ aggiungi**.
Al superamento, Traccar genera **Manutenzione richiesta**, trattato secondo la *policy eventi*
(3.1). I valori sono nelle unità mostrate.

### 3.4 Attributi calcolati (computed)
Deriva un attributo dai dati grezzi del device con un'espressione **JEXL**. Utile per ricavare
l'ignition, il carburante, ecc. da ingressi generici.

**Cookbook (esempi pronti):**
| Scopo | Attributo | Espressione | Tipo |
|---|---|---|---|
| Ignition da voltaggio | `ignition` | `power ? power > 13.2 : null` | bool |
| Ignition da ingresso digitale | `ignition` | `in2 ? in2 == 1 : false` | bool |
| Ignition da bit di `flags` | `ignition` | `flags ? (flags & 2) != 0 : false` | bool |
| Carburante da analogico | `fuel` | `(adc1 ?? null) != null ? adc1 * 0.065 : null` | num |
| Ignition da batteria+velocità | `ignition` | `battery < 4.18 && speed < 1 ? false : true` | bool |
| Eccesso oltre 90 km/h | `speedExcess` | `speed*1.852 > 90 ? speed*1.852 - 90 : 0` | num |

Regole:
- i campi posizione (`speed`, `course`, `latitude`…) ci sono sempre; gli **attributi** solo se
  il device li manda → proteggi con il ternario (`attr ? ... : null`).
- un risultato `null` **rimuove** l'attributo; `??` tiene lo zero come valore.
1. **Attributo** (nome risultato) + **Espressione** + **tipo** (bool/num/testo).
2. **device** + **+ aggiungi**. Vale per tutte le posizioni di quel device.

---

## Parte 4 — Cosa succede in automatico
- **Ricezione doppia**: posizioni/eventi via webhook (forward) **e** polling (backfill +
  rilevazione badge silenti). I due convergono nello stesso incidente (nessun doppione).
- **Eventi → incidenti**: ogni evento è mappato secondo la **policy eventi**; i *critici* aprono
  un incidente e fanno partire la notifica (GoAlert).
- **Presenza**: YouWorkforce confronta le posizioni con le geofence (Mappa) — calcolo nativo,
  non serve configurare geofence in Traccar.
- **Batteria**: la policy batteria (off/warning/critico, in ⚙ Impostazioni sicurezza) decide la
  gravità del livello basso.
- **Watchdog**: badge muto → `perdita_contatto`; Traccar giù → `traccar_conn_down`.

---

## Parte 5 — Procedure rapide (passo-passo)

**A. Collegare un nuovo badge**
1. Registra il badge su Traccar (lato fornitore) con un `uniqueId`.
2. YW → Dispositivi → + nuovo → sorgente traccar, ID esterno = quel `uniqueId`, assegna il lavoratore.
3. Attendi la prima posizione → compare in Mappa.

**B. Rendere critico un evento (es. eccesso velocità)**
1. Connettori → Traccar → 🛠 Traccar avanzato → Policy eventi.
2. *Eccesso velocità* → **critico** → Salva policy.
3. (opz.) Imposta `speedLimit` come attributo del device su Traccar o via computed.

**C. Programmare un tagliando**
1. 🛠 Traccar avanzato → Manutenzioni → Nome, tipo *distanza*, inizio 10000, periodo 15000, device.
2. + aggiungi. Al superamento arriva *Manutenzione richiesta* (secondo la policy).

**D. Vedere dove è stato un lavoratore**
1. Dispositivi → device → Report → 🚗 Viaggi (o 📊 Riepilogo).
2. ⬇ CSV per l'export.

---

## Parte 6 — Risoluzione problemi
| Sintomo | Causa | Rimedio |
|---|---|---|
| "Connessione Traccar non configurata" | manca base URL/credenziali | Superadmin → Connessioni |
| Device non riceve comandi | sorgente ≠ traccar o `uniqueId` errato | correggi il device |
| Nessuna posizione | badge non invia / uniqueId non combacia | verifica su Traccar |
| Report vuoti | nessuna posizione nel periodo | controlla che il device tracci |
| Troppi eventi rumorosi | policy troppo permissiva | metti su *ignora* in Policy eventi |
| Evento importante non allarma | policy su *registra* | mettilo su *critico* |
| Attributo calcolato assente | espressione errata o device non collegato | correggi l'espressione / collega il device |
| Conducente non riconosciuto | ID non combacia col lettore | allinea l'ID RFID/iButton |
| Nessuno avvisato sui critici | nessun trasporto in GoAlert | vedi `docs/goalert-manuale.md` |

---

## Note
- **Notifiche**: la consegna (SMS/voce/Slack) passa da **GoAlert**, non dai notificator Traccar.
- **Geofence**: si disegnano e gestiscono in **YouWorkforce** (Mappa/Piantine), non in Traccar.
- **Filtri posizione / reverse-geocoding**: configurati lato server dal superadmin.
- **Fonte dati**: drivers, manutenzioni e attributi calcolati vivono **in Traccar** (YW li
  gestisce come proxy): un'unica fonte, niente copie disallineate.
