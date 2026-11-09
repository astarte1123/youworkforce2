# Modulo Traccar — guida alla configurazione (per amministratori)

Tutto si configura **da YouWorkforce**, mai dalla UI di Traccar. Due livelli di
responsabilità: il **superadmin** abilita e connette; il **tenant_admin** regola e
instrada; **operatore/auditor/lavoratore** lo usano.

## Chi configura cosa

| Attore | Dove (YouWorkforce) | Cosa configura |
|---|---|---|
| **superadmin** | Piano di controllo → tenant | Abilita il modulo `traccar`; **Connessione Traccar** (base URL + utente + password); Strumenti disponibili; Config server Traccar (runtime `/api/server`) |
| **tenant_admin** | Console → Connettori | **Verifica collegamento**; **Impostazioni sicurezza** (cadenza poll, soglia silenzio badge, soglia down connessione, poll on/off) |
| **tenant_admin** | Console → Moduli → Traccar → Configura strumenti | Mappatura allarmi (`alarm_map`): SOS→sos/man_down, offline critico sì/no, fermo→immobilità, soglia batteria + **policy batteria** (off/warning/critico), filtri posizione (salto max, movimento minimo) |
| **tenant_admin** | Console → Gruppi allarme | Routing **ogni tipo di allarme → squadra** (sos, man_down, immobilità, perdita_contatto, manomissione_badge, perdita_alimentazione, jamming, traccar_conn_down…) |
| **tenant_admin** | Console → Dispositivi / Connettori | Device (CRUD, provisioning, catalogo); geofence; token di ingest |
| **operatore** | Console → Centrale / Mappa | Ack/chiudi incidenti; mappa live; comandi al device (localizza, stop motore…) |
| **auditor** | Console (sola lettura) | Vede tutto, non modifica |
| **lavoratore / reperibile** | App PWA | Indossa il badge; visibilità per-sede (ACL); il reperibile riceve l'escalation |

## Prima configurazione (onboarding di un tenant su Traccar)
1. **superadmin**: tenant → abilita modulo `traccar` → **Connessione Traccar** → base URL, utente, password → *Salva e verifica*.
2. **tenant_admin**: Connettori → **Verifica collegamento** (verdetto "collegato", device specchiati). La stessa card mostra la config di forward opzionale da incollare nell'avvio del container Traccar (fast-path a bassa latenza; non obbligatorio, il pull copre tutto).
3. **tenant_admin**: **Impostazioni sicurezza** → regola le soglie (vedi sotto).
4. **tenant_admin**: Moduli → Traccar → mappatura allarmi; Gruppi allarme → instrada ogni allarme alla squadra giusta.

## Parametri di sicurezza (chi decide quando scatta l'allarme)
Configurabili dal tenant_admin in Connettori → Impostazioni sicurezza:

| Parametro | Default | Effetto |
|---|---|---|
| Cadenza poll | 30 s | Ogni quanto interroghiamo Traccar (5–600 s) |
| Soglia silenzio badge | 600 s | Oltre questo silenzio di un badge → incidente **perdita_contatto** (0 = watchdog badge off) |
| Soglia down connessione | 180 s | Traccar irraggiungibile oltre questo → incidente **traccar_conn_down** + escalation (30–3600 s) |
| Poll attivo | sì | Spegne/accende il pull (lo storico resta) |

Policy batteria (in Moduli → Traccar → `alarm_map`): **off** (ignora) · **warning** (evento
`batteria_bassa`, default, non critico) · **critico** (`batteria_critica` → incidente +
escalation). Soglia % impostabile con `battery_min`.

## Come funzionano gli allarmi (catena)
- **Pull (spina dorsale):** interroghiamo Traccar via API con un cursore: recuperiamo gli eventi anche se il core era giù. Non serve alcuna config lato Traccar oltre alle credenziali.
- **Push (fast-path opzionale):** il forwarding di Traccar verso di noi riduce la latenza; va impostato all'avvio del container. I due convergono senza duplicare.
- **Watchdog badge:** badge silente oltre soglia → `perdita_contatto` anche se Traccar non manda `deviceOffline`.
- **Watchdog connessione:** Traccar irraggiungibile oltre soglia → `traccar_conn_down` + escalation; al ripristino l'incidente si chiude da solo. **Il sistema non resta mai cieco in silenzio.**
- Ogni allarme critico apre un incidente e richiede l'escalation a GoAlert; la chiusura dell'incidente chiude anche l'alert GoAlert (close-loop).

## Allarmi Traccar mappati
`sos`→sos/man_down · `fallDown/accident`→man_down · `removing/tampering`→manomissione_badge ·
`powerCut`→perdita_alimentazione · `jamming/gpsAntennaCut`→jamming ·
`lowBattery`/soglia %→**policy batteria** (off / batteria_bassa warning / batteria_critica allarme) ·
`deviceOffline`→perdita_contatto (opt-in) · `deviceStopped`→immobilità (opt-in) · geofence→presenza.

## Copertura Man Down / SOS (D.Lgs. 81/08, artt. 17/43/45)
| Evento richiesto | Nel modulo Traccar | Chi lo gestisce |
|---|---|---|
| Caduta / posizione orizzontale prolungata | `fallDown`/`accident`→man_down (critico, escalation) | rilevato dal device, mappato+escalato da noi |
| Immobilità (assenza movimento) | `deviceStopped`→immobilità (opt-in, critico) | device + nostra config |
| Perdita segnale / badge silente | watchdog silenzio→perdita_contatto (critico) | **nostro**, indipendente da deviceOffline |
| Traccar irraggiungibile (sistema cieco) | watchdog connessione→traccar_conn_down (critico) | **nostro** |
| Batteria in esaurimento | policy batteria off/warning/critico | device + nostra config |
| SOS / panic button | `sos`→sos/man_down (critico) | device/app, escalato da noi |
| Check-in ignorato | mancato_checkin (critico) | modulo check-in PWA (non Traccar) |
| Geolocalizzazione / beacon indoor | posizione sincronizzata, incidente con lat/lon | device che invia fix GPS/rete/beacon |
| Smistamento (centrale, RSPP, preposti, colleghi) | Gruppi allarme: ogni etype → squadra GoAlert (SMS/voce/push, on-call) | tenant_admin configura |
| Gestione falso allarme | ack operatore + chiusura con **esito** (registrato) | operatore |
| Post-evento / audit / DVR | incident_timeline completo (notify_requested, goalert_sent, human_ack, transition+esito, goalert_closed) + comandi + posizione | tracciato automaticamente |

**Finestra di annullamento (15–30 s) — dov'è:** il countdown "ti sei rialzato? annulla"
è responsabilità del **device/app** (badge, smartwatch, PWA): il dispositivo suona, e invia
SOS/fallDown a Traccar **solo se non annullato**. Quando l'allarme arriva a noi la finestra è
già trascorsa, quindi NON aggiungiamo un ritardo server-side: introdurrebbe latenza su un SOS
reale. La gestione del falso allarme lato backend avviene con ack + chiusura con esito.

## Note di sicurezza (comportamenti voluti e limiti noti)
- **`perdita_contatto` non si auto-chiude**: un badge che torna online non significa
  "lavoratore al sicuro". L'incidente resta aperto fino all'**ack dell'operatore** (scelta
  di sicurezza, non un bug).
- **Badge mai acceso** (nessun `lastUpdate` su Traccar): il watchdog silenzio non ha una
  base temporale, quindi non genera `perdita_contatto`. Questo caso è coperto dal
  **provisioning** (un device che non raggiunge "attivo al primo fix" è visibile lì).
- **Degrado di Traccar** (autentica ma risponde 5xx sui dati): trattato come
  irraggiungibile → scatta `traccar_conn_down` (non resta mai cieco in silenzio).
- **GoAlert irraggiungibile a lungo**: l'outbox ritenta con **backoff esponenziale**
  (5s→300s). Gli invii di sicurezza (`goalert_notify`/`goalert_close`) **non vengono mai
  abbandonati**: ritentano a oltranza finché non passano, così un outage di GoAlert non può
  far sparire un SOS. Gli invii non critici capitolano in `error` dopo 6 tentativi. Le
  notifiche bloccate sono visibili nella card salute (`outbox_stuck` + ultimo errore) e
  segnalate sul bus. Un canale di trasporto alternativo a GoAlert resta un'estensione futura.

## Verifica (terminale)
```bash
bash qa/traccar-poll-e2e.sh       # pull, cursore, watchdog badge, verifica, salute
bash qa/traccar-conndown-e2e.sh   # watchdog connessione: down -> allarme -> recovery
bash qa/traccar-sync-e2e.sh       # sync device + comandi tracciati
bash qa/crypto-e2e.sh             # credenziali cifrate a riposo
bash qa/traccar-battery-policy-e2e.sh  # policy batteria off/warning/critico
bash qa/outbox-resilience-e2e.sh  # #6: escalation critica mai abbandonata + backoff
```
