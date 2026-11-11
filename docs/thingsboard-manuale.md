# IoT / ThingsBoard — Manuale YouWorkforce

Guida per l'amministratore. Come **configurare** e **usare** l'integrazione ThingsBoard da
YouWorkforce. Split dei ruoli:
- **Superadmin**: connessione (base URL + credenziali, cifrate).
- **Tenant admin**: operatività (regole, policy allarmi, inventario, comandi, config remota, storico).

ThingsBoard è la piattaforma IoT integrata: raccoglie la telemetria dei sensori e i suoi allarmi.
YouWorkforce li **pulla** (+ riceve push), applica le **regole sensori** e gli **allarmi nativi
TB**, e apre incidenti con notifica via GoAlert.

---

## Parte 1 — Configurazione (superadmin)
**Superadmin → tenant → Connessioni & integrazioni → ThingsBoard**:
1. Attiva il modulo **thingsboard**.
2. **base URL** (es. `https://thingsboard.tuodominio` o `http://host:8080`), **utente** e
   **password** di un account ThingsBoard di tenant.
3. **Verifica**: il semaforo conferma la raggiungibilità.

Credenziali cifrate (write-only). Il tenant admin non le vede né le modifica.

---

## Parte 2 — Operatività (tenant admin)
Tutto in **Connettori → ThingsBoard**.

### 2.1 Salute + sync
La card mostra stato (raggiungibile, device, ultimo pull). Pulsante **🔄 Sincronizza ora**:
specchia i device nell'inventario, pulla l'ultima telemetria e gli allarmi nativi TB. (In
background il pull gira anche in automatico alla cadenza impostata.)

### 2.2 Regole sensori
**Connettori → Regole sensori**: trasformano la telemetria in allarmi in modo parametrico —
**metrica** (chiave nel payload) + **operatore** + **soglia** → **evento**. Gli eventi critici
aprono incidente + escalation. Valgono sia per ThingsBoard sia per l'IoT generico. Il pulsante
**✦ Suggerisci regole (AI)** propone regole dai dati.

### 2.3 ThingsBoard avanzato
**Connettori → ThingsBoard → 🛠 ThingsBoard avanzato**.

**Policy allarmi (per severità)** — gli allarmi **nativi di ThingsBoard** (definiti nei device
profile TB: soglie, durata, ripetizione) entrano in YouWorkforce secondo questa policy:
- **ignora**: non registrato.
- **registra**: evento non critico.
- **critico**: allarme + escalation (come un SOS).

Default safety-first:
| Severità TB | Default |
|---|---|
| CRITICAL | critico |
| MAJOR | critico |
| MINOR | registra |
| WARNING | registra |
| INDETERMINATE | ignora |

> Così copri sia le **tue regole** (sezione 2.2, sulla telemetria) sia gli **allarmi TB-side**
> già configurati nei device profile.

**Device ThingsBoard** — per ogni device:
- **Attributi**: stato/config correnti (client/shared/server).
- **Comando (RPC)**: invia un comando al device (es. `setGpioStatus` con parametri JSON).
  One-way (fire-and-forget). Se il device è offline la risposta lo segnala.
- **Config remota**: scrive *shared attributes* (es. `{"reportInterval":60}`) → il device li
  riceve e si riconfigura.
- **Storico**: serie storica di una o più metriche nel periodo.

---

## Parte 3 — Cosa succede in automatico
- **Pull + push**: la telemetria/eventi arrivano via webhook (Rule Engine TB →
  `/api/ingress/thingsboard`) e via polling (specchio device + ultima telemetria + allarmi).
  Convergono sulla stessa catena incidenti (nessun doppione: dedup per id alarm/evento).
- **Regole**: ogni telemetria passa per le *regole sensori*; gli allarmi nativi TB per la
  *policy allarmi*. I critici aprono incidente e fanno partire GoAlert.
- **Inventario**: i device TB sono specchiati automaticamente a ogni sync.

---

## Parte 4 — Procedure rapide
**A. Collegare un sensore**
1. Il sensore invia a ThingsBoard (lato deployment TB).
2. YW → Connettori → ThingsBoard → **Sincronizza ora** → il device compare nell'inventario.
3. (opz.) Aggiungi una **regola sensore** sulla sua metrica.

**B. Far scattare un allarme dalla telemetria**
1. Connettori → Regole sensori → Nuova regola: metrica `gas`, operatore `≥`, soglia `50`,
   evento *Sensore critico* → + Aggiungi.
2. Alla prossima lettura oltre soglia parte l'incidente.

**C. Ingerire gli allarmi già configurati in ThingsBoard**
1. 🛠 ThingsBoard avanzato → Policy allarmi → scegli come trattare ogni severità → Salva.
2. Al prossimo sync gli allarmi attivi TB diventano incidenti secondo la policy.

**D. Riconfigurare un device da remoto**
1. 🛠 ThingsBoard avanzato → device → **Config remota** → `{"reportInterval":30}` → invia.

---

## Parte 5 — Risoluzione problemi
| Sintomo | Causa | Rimedio |
|---|---|---|
| "connessione ThingsBoard non configurata" | manca base/credenziali | Superadmin → Connessioni |
| Nessun device dopo il sync | account TB senza device / credenziali errate | verifica su ThingsBoard |
| Allarmi TB non entrano | policy su *ignora* o sync non eseguito | Policy allarmi + Sincronizza |
| Telemetria non fa scattare nulla | manca la regola sensore | Regole sensori |
| RPC "HTTP 409" | device offline | il comando parte alla riconnessione (persistente) o riprova |
| "device non ancora sincronizzato (manca tb_id)" | device mai specchiato | Sincronizza ora |

---

## Note
- **Notifiche**: la consegna (SMS/voce/Slack) passa da **GoAlert**, non dai notificator TB.
- **Dashboard/cruscotti**: usiamo i nostri (Cruscotti IoT), non le dashboard TB.
- **Rule Engine / Calculated Fields TB**: l'equivalente nostro sono le **regole sensori**.
- **Provisioning/claiming, OTA, Edge, integrazioni/converter**: spettano al deployment TB
  (device→piattaforma), non al consumer YouWorkforce.
