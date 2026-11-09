# Accesso e collaudo — utenti di test

Tutto gira in locale. **Come in produzione, il tenant deriva dal sottodominio**:
apri `http://acme.localhost:8081` (o `http://globex.localhost:8081` per provare
l'isolamento) e il campo **Tenant** si compila da solo (sola lettura). I browser
risolvono `*.localhost` → 127.0.0.1 da soli (RFC 6761): nessuna modifica al file
`hosts` necessaria. Su `http://localhost:8081` nudo il campo Tenant resta
modificabile (comodità dev).

> Il core va avviato con `YW_BASE_DOMAIN=localhost` e `YW_CONTROL_HOST=tenants.localhost`
> (in produzione: il dominio reale e `tenants.<dominio>`). Imposta anche
> `YW_SECRET_KEY=<passphrase>` per cifrare a riposo i segreti upstream (password
> Traccar, gql_pass GoAlert); senza, restano in chiaro e il log avvisa.

## URL (prod-like, per-sottodominio)
| Cosa | URL |
|---|---|
| Console tenant acme | http://acme.localhost:8081 |
| Console tenant globex (isolamento) | http://globex.localhost:8081 |
| App lavoratore (PWA mobile) | http://acme.localhost:8081/worker/ |
| Piano di controllo (superadmin) | http://tenants.localhost:8081/superadmin.html |
| GoAlert (escalation reale) | http://localhost:8082 — `admin` / `goalertadmin1` |
| Traccar (badge reale) | http://localhost:8083 — `op@acme.test` / `Passw0rd!` |
| ThingsBoard (IoT reale) | http://localhost:8084 — `tenant@thingsboard.org` / `tenant` |

## Utenti di test (tenant `acme`, password `demo`)
| Ruolo | Email | Cosa può fare |
|---|---|---|
| Admin tenant | admin@acme.loneworker.eu | tutto nel tenant |
| Operatore centrale | operatore@acme.loneworker.eu | vede/gestisce incidenti (ack, chiudi), AI |
| Pianificatore | pianificatore@acme.loneworker.eu | crea/assegna ordini |
| Responsabile | responsabile@acme.loneworker.eu | approva consuntivi |
| Lavoratore (PWA) | mario@acme.loneworker.eu | SOS, check-in, turni, note, ordini |
| Auditor (sola lettura) | auditor@acme.loneworker.eu | legge, non può ack/approvare |
| Superadmin | admin@loneworker.eu / **admin** | crea tenant, moduli, routing |
| Isolamento | admin@globex.loneworker.eu | tenant diverso: non vede acme |

## 5 cose da provare in 5 minuti (solo browser)
1. **Catena SOS live**: apri la **Console** come `operatore` (tenant acme) e lasciala aperta
   sulla Centrale. In un'altra scheda apri l'**App lavoratore** come `mario`, premi **SOS**.
   → l'incidente compare **in tempo reale** nella centrale (SSE). Fai **Presa in carico** e **Chiudi**.
2. **Turni + nota vocale**: nell'app lavoratore premi **Inizia turno** (stato confermato dal
   server), poi **Detta una nota**: usa il microfono (riconoscimento vocale del browser) o
   scrivi a mano, salva. Riapri l'app: la nota resta; offline resta in coda con indicatore.
3. **Lavoro assegnato**: Console come `pianificatore` → crea un ordine, assegnalo a **mario** →
   nell'app di mario appare in **Oggi** → avvia e completa → crea il consuntivo (ore+spesa) →
   Console come `responsabile` → approva → esporta **CSV**.
4. **Moduli per tenant senza deploy**: apri **superadmin** → tenant **acme** → spegni/accendi
   un modulo (es. `traccar`): l'effetto è immediato su UI e API, senza riavvii.
5. **Isolamento**: apri la console come `admin@globex` → non vedi nessun dato di acme.
6. **Configurazione strumenti per-modulo** (due livelli):
   - come `admin@acme` → menu **Moduli** → su un modulo attivo premi **Configura strumenti**:
     form generato dallo schema (es. loneworker → *Timer check-in*: minuti predefiniti/massimi).
     Salva. Prova: dall'app lavoratore crea un check-in → rispetta i tuoi valori.
   - come **superadmin** → su un tenant premi **Strumenti disponibili**: accendi/spegni i singoli
     strumenti. Spento → sparisce dalla config del tenant_admin. (superadmin abilita, admin configura)
7. **Gestione del tenant (Fase 1)** — login `admin@acme.loneworker.eu` / `demo`, menu a sinistra:
   - **Dispositivi**: + Nuovo dispositivo (badge/sensore), assegna a sede/lavoratore, abilita/disabilita.
   - **Gruppi allarme**: crea gruppo, aggiungi membri, imposta routing evento→gruppo (sos, man_down…).
   - **Utenti e sedi**: invita utenti con ruolo, cambia ruolo/password, elimina; crea/rinomina/elimina sedi.
   - **Centrale allarmi**: apri un incidente → **Assegna** operatore + **Nota**; pannello **Check-in attivi**.
8. **Forza lavoro e turni (Fase 2)** — menu **Turni** (come `pianificatore@acme` o admin):
   - **Pianifica turno** (lavoratore, sede, inizio/fine, nota) → il lavoratore lo vede e lo **conferma** (anche dall'app PWA).
   - **Reperibilità**: finestre on-call per utente.
   - **Squadre**: in **Utenti e sedi** → crea squadra, aggiungi/rimuovi membri.
9. **Interventi pianificati (Fase 3)** — menu **Ordini di lavoro** (come `pianificatore@acme`):
   - **+ Nuovo ordine** con sede, assegnatario, **finestra (inizio/fine)** e **priorità** → compare in **Agenda**.
   - Apri un ordine → **Modifica / ripianifica** (cambia finestra/priorità/assegnatario, concorrenza ottimistica).
   - L'app lavoratore mostra orario e priorità sull'ordine.
10. **Connettori e IoT (Fase 4)** — menu **Connettori** (come `admin@acme`):
    - Riepilogo per connettore (Traccar/ThingsBoard/GoAlert/IoT): stato modulo, token, endpoint.
    - **Credenziali di ingest**: genera/copia/revoca i token macchina (il tenant deriva dal token).
    - **Dispositivi** → colonna **Ultimo contatto** (telemetria/eventi ricevuti).
    - **Mappa** (operatore/admin): posizioni dei device su **OpenStreetMap** (Leaflet), popup con
      batteria/ora/assegnatario, **percorso recente**, aggiornamento **live**. Il badge invia GSM/GPS via
      *position forwarding* Traccar. Opzione `stopped_as_immobilita` in Moduli→Traccar (fermo→incidente).

## Provare gli upstream REALI (terminale)
```bash
# SOS da "badge" reale: device -> Traccar -> core -> GoAlert
curl "http://localhost:5055/?id=BADGE-001&lat=45.47&lon=9.19&timestamp=$(date +%s)&alarm=sos"
# telemetria critica IoT: device -> ThingsBoard Rule Engine -> core
#   (token device in qa/run-thingsboard.sh; o manda da UI ThingsBoard)
# consegna reale notifica di 2° livello (webhook verificato):
bash qa/goalert-delivery.sh
# GoAlert P1+P2+P3 (close-loop, stato ack/close, on-call, provisioning, heartbeat):
bash qa/goalert-p123-e2e.sh
# GoAlert config trasporto + ack/escalate + validazione Twilio (da YouWorkforce):
bash qa/goalert-config-e2e.sh
bash qa/goalert-p4-e2e.sh
# Config server Traccar da YouWorkforce:
bash qa/traccar-config-e2e.sh
# Traccar P1 pull: polling via API con cursore (recupero eventi persi) + watchdog
# silenzio (perdita_contatto senza dipendere da deviceOffline) + salute connessione:
bash qa/traccar-poll-e2e.sh
```

## Tutte le prove automatiche (evidenza)
```bash
bash qa/smoke.sh                 # 34  catena critica, isolamento, moduli
bash qa/prompt8-e2e.sh           # 11  turni + diario
bash qa/modularity-e2e.sh        # 8   moduli per-tenant anche sugli ingress
bash qa/module-gating-e2e.sh     # 6   modulo spento -> gestione sparisce da UI e API
bash qa/module-config-e2e.sh     # 10  config strumenti per-modulo (2 livelli + validazione)
bash qa/config-consumers-e2e.sh  # 6   la config guida il comportamento (priorita SOS, arrotondamenti, turni)
bash qa/mgmt-e2e.sh              # 20  gestione: dispositivi, utenti, sedi, gruppi, assegna/note, check-in
bash qa/workforce-e2e.sh        # 14  squadre, turni pianificati (conferma/annulla), reperibilita
bash qa/interventi-e2e.sh       # 8   ordini pianificati (agenda, priorita) + modifica con concorrenza
bash qa/iot-mgmt-e2e.sh         # 8   connettori: ingest-token (crea/usa/revoca), stato dispositivi
bash qa/traccar-p1-e2e.sh       # 13  Traccar P1: mappa allarmi sicurezza, offline critico, batteria
bash qa/traccar-p2-e2e.sh       # 12  Traccar P2: posizioni (ultima nota+storico), geofence presenza
bash qa/device-attrs-e2e.sh     # 3   #1 attributi device (ignition/heartRate/steps/temp)
bash qa/geocode-e2e.sh          # 4   #4 geocoding inverso (indirizzo da coord, cache)
bash qa/traccar-cmd-e2e.sh      # 4   #2 comandi al device via API Traccar (localizza)
bash qa/geofence-e2e.sh         # 10  #3 geofence CRUD + presenza (dentro/fuori)
bash qa/reports-e2e.sh          # 5   #5 report percorso/soste/presenza + CSV
bash qa/posfilter-e2e.sh        # 5   #7 filtri posizione (salti/rumore)
bash qa/acl-e2e.sh              # 6   #8 ACL per-sede (operatore ristretto)
bash qa/device-onboarding-e2e.sh # 11 catalogo + wizard + asset + attivo-al-primo-fix
bash qa/sse-scale.sh             # 2   eventi live via NOTIFY/LISTEN
bash qa/traccar-e2e.sh           # 8   badge reale
bash qa/thingsboard-e2e.sh       # 8   IoT reale
bash qa/goalert-delivery.sh      # 10  consegna reale
bash qa/auth-hardening.sh        # 5   argon2id
bash qa/session-hardening.sh     # 7   sessioni persistenti
bash qa/ratelimit-hardening.sh   # 4   anti brute-force
bash qa/csrf-hardening.sh        # 5   CSRF
```

> Nota: è una **DEMO con upstream reali**. Recapiti di prova (nessun numero reale).
> Non usare numeri d'emergenza. `TEST_CLOCK=1` è attivo (disattivare fuori dai test).
