# Console YouWorkforce — Guida all'interfaccia

Questa guida descrive le funzionalità di usabilità della console tenant (`app.html`,
`http://localhost:8081`) e, dove indicato, della console superadmin (`superadmin.html`).
Riguarda solo il front-end: nessuna modifica al backend, nessuna migration.

> Dopo un aggiornamento della console: **ricarica forzata** della pagina (`Ctrl+Shift+R`).
> `app.html` è servito `no-store` (sempre fresco), ma `app.js`/`app.css` sono cacheabili dal browser.

---

## 1. Ricerca nelle combo (tutta la piattaforma)

Ogni menu a tendina (`<select>`) della console — sia app tenant sia superadmin — è una
**combo con ricerca**: ci si scrive dentro e le opzioni vengono filtrate.

- **Filtro multi-parola** in AND (es. `teltonika sos` mostra solo le opzioni che contengono
  entrambe le parole).
- **Tastiera**: ↑/↓ naviga, **Invio** seleziona, **Esc** chiude. Click fuori chiude.
- Mostra "nessun risultato" se niente corrisponde.
- Il menu aperto si sovrappone (**copre**) agli elementi sottostanti, comprese le mappe
  (es. le combo "Piantina" e "Posiziona sulla piantina" sopra la piantina indoor).
- Il `<select>` nativo resta la **sorgente di verità**: il valore selezionato e tutte le
  logiche collegate non cambiano.

Implementazione: enhancer unico in `console/static/app.js` (caricato da entrambe le pagine),
applicato automaticamente a ogni render. Nessuna libreria esterna. Per escludere un singolo
select si può aggiungere l'attributo `data-nocombo`.

---

## 2. Dispositivi & App — ricerca full-text

Sopra la tabella dispositivi c'è un **unico campo di ricerca** che filtra su tutte le colonne:
**ID esterno, Modello** (produttore/nome/codice, + "generico"), **Etichetta, Assegnato a**
(asset o email), **Capacità, Ultimo contatto** (data o "mai"), **Onboarding**
(attivo/attesa/registrato/configurato/off).

- Multi-parola in AND, filtro istantaneo (client-side), con **conteggio** live ("12 di 48")
  e messaggio "nessun risultato".

---

## 3. Mappe separate: Allarmi e Dispositivi

La vecchia pagina **Mappa** (con toggle interno) è stata divisa in due voci di menu:

- **Mappa allarmi** (gruppo *Sicurezza lavoratore*): solo incidenti georeferenziati
  (indoor/outdoor, aree).
- **Mappa dispositivi** (voce condivisa fra *Sicurezza lavoratore* e *Lavoro sul campo*):
  dispositivi live, geofence, presenza. È condivisa perché i device (es. Teltonika GH-5200)
  generano allarmi SOS/man-down **e** sono asset di campo.

La **gestione delle aree geofence** (disegno ⬡, rinomina, colore, modifica forma, associa
device/eventi, elimina) è riservata ai `tenant_admin` e ha visibilità differenziata:

- **Mappa dispositivi** (lavori sul campo): **sempre disponibile**.
- **Mappa allarmi** e **Piantine**: **nascosta di default** (non serve al perimetro
  sicurezza). Il `tenant_admin` può attivarla con il pulsante **▢ Mostra aree geofence**
  nella barra (e **▣ Nascondi** per rimetterla via). La scelta è **per-tenant** e persistente
  (flag `geofence_alarms`, endpoint `POST /api/ui/features`, default `false`).

Quando attiva, nella Mappa allarmi il campo **"Colore area Geofence"** è allineato agli altri
campi della barra (Dal / Al / Tipo).

### Piantine — posizionamento unificato

Nelle Piantine il posizionamento di **beacon** e **dispositivi** usa un **unico campo**
"Posiziona sulla piantina": un solo selettore (**ricercabile**, con le voci **raggruppate**
in *📡 Beacon (zone)* e *📟 Dispositivi*) e il pulsante **"📍 sulla mappa" accanto** al
selettore. Il tipo si deduce dalla voce scelta e instrada all'endpoint corretto. Restano due
entità distinte: il **beacon** è un'ancora fissa che definisce una **zona** (gli allarmi indoor
vi si agganciano), il **dispositivo** è l'asset tracciato.

Nella barra della piantina il campo **"Colore area Geofence"** è allineato agli altri; il
pulsante **"⚙ Configura piantina indoor"** sta in alto a **destra**, sopra il pannello di
dettaglio.

### Selezione allarme dall'elenco
Nella Mappa allarmi, cliccando una riga dell'elenco a destra:
- la mappa si **centra/zooma** sul pin e lo evidenzia con un **anello pulsante** (animato in JS);
- appare un **tooltip compatto** sul pin (tipo, orario, stato, persona, device) — non il popup
  completo, che resta disponibile cliccando direttamente il pin;
- la riga viene **selezionata** nell'elenco;
- gli allarmi **senza coordinate GPS** (beacon non mappato) sono comunque cliccabili e aprono
  direttamente l'incidente.

---

## 4. Centrale allarmi — ricerca, tab, paginazione

La Centrale può contenere moltissimi incidenti. Per tenerla ordinata:

- **Tab segmentati**: **Aperti** · **Chiusi** · **Tutti**, ciascuno con il **conteggio**.
  Default = *Aperti* (gli attuabili).
- **Ricerca full-text** accanto ai tab: filtra nel tab corrente su **Tipo** (+ etichetta),
  **Sede, Assegnato, Stato** (+ sinonimi), **Priorità** (numero + livello bassa/media/alta/critica),
  **Correlazione**.
- **Paginazione** 25/pagina: `‹ Prec · Pagina N/K · Succ ›` + "Mostrando X–Y di Z".
- **Ordinamento**: gli aperti per priorità decrescente; Chiusi/Tutti nell'ordine del backend.
- Tornando sulla pagina, **ricorda** tab e ricerca.

Paginazione e filtro sono client-side; per volumi enormi (decine di migliaia) si potrà spostare
la paginazione sul server in futuro.

---

## 5. Centrale allarmi — allarme live (popup + suono + riga lampeggiante)

Quando un **dispositivo apre un incidente** (evento SSE `incident_opened`), oltre agli eventi
già esistenti (toast di notifica, escalation/notifiche lato backend — **invariati**):

- **Suono**: beep d'allarme a tre impulsi, generato via Web Audio (nessun file esterno).
  Il browser sblocca l'audio dopo la prima interazione dell'operatore (policy autoplay).
- **Popup**: modale rosso "🚨 NUOVO ALLARME" con tipo, correlazione e ora. Se arrivano più
  allarmi mentre è aperto, li **accumula** (badge "N allarmi"). Pulsanti **Chiudi** e
  **Vai alla Centrale**; cliccando una riga si apre l'incidente. Appare da qualsiasi pagina.
- **Riga lampeggiante**: gli incidenti ancora **aperti** (`aperto`, non ancora presi in carico)
  hanno la riga **evidenziata e pulsante** (sfondo rosso, barra laterale, tipo in rosso),
  distinta dalle altre. Smette quando l'allarme passa a *preso in carico* o *chiuso*.
  Rispetta `prefers-reduced-motion` (resta evidenziata senza lampeggio).

---

## 5bis. Escalation e reperibilità — a tab

La pagina (molto densa) è organizzata in **tab** in cima, un pannello per volta:

- **Gruppi**: gruppo di default (catch-all) + elenco gruppi/membri/rotazione. Le funzioni
  per-gruppo sono su una **striscia di sotto-tab** dentro ogni gruppo
  (**⚙ Copertura·handoff·Slack · ⛓ Policy · 🏷 Label · 📋 Playbook · 👥 Assegnazioni ·
  📅 Turni**): si apre un pannello per volta, ri-click chiude; i pannelli pesanti si caricano
  solo all'apertura. Niente più sei fisarmoniche impilate.
- **Routing**: evento → gruppo.
- **Notifiche**: fallback vocale, aggiornamenti al reperibile, manutenzione (sospende
  escalation), calendario reperibilità, diagnostica motore.
- **Recapiti**: recapiti dei reperibili + verifica OTP.
- **Assenze & turni**: assenze/sostituzioni, pianificazione temporanea.
- **Monitor**: diagnostica consegne/parità, heartbeat monitor, ingress email → allarme.

La riorganizzazione è solo visiva: logica, ID e azioni invariati.

## 5ter. Tema (chiaro/scuro)

Il pulsante tema in topbar mostra l'icona **🌙 luna con tema chiaro** e **☀️ sole con tema
scuro** (si aggiorna anche se il sistema cambia tema da solo).

## 6. Responsive (mobile / tablet)

- **Navigazione mobile**: sotto 820px la barra laterale diventa un **drawer a scomparsa**.
  In alto a sinistra compare il pulsante **☰** (hamburger) che apre/chiude il menu; uno
  sfondo scuro (scrim) chiude al tocco e il menu si chiude anche scegliendo una voce.
  (Prima la barra veniva **nascosta** su mobile, rendendo l'app non navigabile da telefono.)
- **Tabelle**: su schermi stretti (≤640px) le tabelle **scorrono in orizzontale** dentro la
  loro card invece di sforare la pagina.
- **Griglie e barre**: KPI e colonne si impilano progressivamente; **sotto 480px le griglie a
  schede passano a una sola colonna** (es. Permessi, Ruoli personalizzati, Gruppi, Ronde); le mappe
  passano a una sola colonna (mappa sopra, dettaglio sotto); le barre strumenti e l'intestazione di
  pagina (titolo + azioni) vanno a capo.
- **Tabelle**: tutte le tabelle dentro le card (anche annidate) scorrono in orizzontale su schermi
  stretti, senza sforare la pagina.

## 7. Lingua (i18n IT/EN)

La console ha un selettore **IT/EN** in alto a destra (topbar) e nel login; la scelta è salvata e
ricaricata. Due meccanismi, complementari:

1. **`t('chiave','testo IT')`** per le schermate chiave (login, menu, Centrale, Dispositivi, Mappe,
   Piantine, titoli pagina): con EN usa la traduzione, altrimenti l'italiano (fallback sicuro).
2. **Traduttore runtime** (`applyI18n` + `MutationObserver` + dizionario `I18N_PH`): quando la
   lingua è EN, traduce automaticamente il **testo statico dell'intera UI** a ogni render (titoli,
   intestazioni di card, bottoni/link, colonne, etichette, `placeholder`, `title`), comprese le viste
   non ancora migrate a `t()` e i pannelli/drawer aperti dopo. I **dati dinamici** (valori, numeri)
   non combaciano col dizionario e restano invariati; gestisce anche le intestazioni con contatore
   (es. *Teams (3)*).

Per tradurre una nuova frase: aggiungere una riga `"Testo IT":"English"` in `I18N_PH` (in
`app.html`). Nessuna modifica ai template necessaria.

## 8. Report

Le statistiche in testa al report allarmi (**Totale, Critici, Aperti, Presi in carico,
Tempo medio risposta**) sono disposte su una **griglia a 5 colonne** che riempie tutta la
larghezza (si impila progressivamente su schermi stretti), invece di essere addossate a
sinistra. Il "Tempo medio risposta" mostra un'icona di **orologio (🕐)** quando il dato non è
ancora disponibile (nel PDF resta "—").

## 9. Lavoro sul campo — intervento (console + PWA)

- **Sedi** (Utenti e sedi): oltre al nome, una sede ha **indirizzo, coordinate** (lat/lon) e
  **indicazioni d'accesso** (es. "chiave in portineria"); queste ultime sono **visibili solo agli
  autorizzati** (pianificatore/admin e il tecnico assegnato). Funziona anche con sedi solo-nome.
- **Nuovo ordine**: si può allegare una **checklist** (una voce per riga).
- **PWA «Oggi»** (`/worker/`): il tecnico apre l'intervento e registra **arrivo** e **avvio**
  separati, spunta la **checklist**, scrive **note/report**, **ore** e **materiali**, carica
  **foto** (online) e sceglie l'**esito**, poi invia il rapporto. Le note d'accesso compaiono in
  evidenza. Se la rete cade, la **bozza di testo** resta in coda sul dispositivo (stato "in
  attesa") e si sincronizza da sola; **le foto restano online** (non salvate offline).
- **Revisione**: dal dettaglio ordine in console il responsabile vede esecuzione (arrivo/avvio/
  ore/checklist/materiali/esito), report, allegati e firma.

## 10. Gruppi di persone condivisi (Utenti e sedi)

In **Utenti e sedi** c'è la card **«Gruppi di persone»**: un **insieme di persone** riusabile da
qualunque modulo, **additivo** e distinto dai tre raggruppamenti esistenti (gruppi di **allarme**
= escalation, **squadre** = Workforce, **profili di protezione** = Sicurezza), che restano
invariati.

- **Crea / rinomina / elimina** gruppo (nome + *tipo* libero: squadra/reparto/zona/turno…).
- **Membri**: aggiungi/togli persone del tenant.
- **→ Importa in gruppo allarme**: copia i membri del gruppo in un **gruppo di allarme**
  (alimenta instradamento/escalation). È **idempotente** (non duplica) e **disaccoppiato**:
  eliminare il gruppo condiviso **non** svuota il gruppo di allarme già popolato. L'import
  richiede un modulo escalation attivo (loneworker/traccar/iot/thingsboard); senza, risponde 403.

Solo **tenant_admin** gestisce i gruppi (controllo lato backend). Isolati per tenant (RLS):
un tenant non vede né modifica i gruppi di un altro. Backend: tabelle `app.user_groups` /
`app.user_group_members` (mig `0072`), API `/api/user-groups*` e
`POST /api/groups/:id/members/import`.

## Note tecniche
- File coinvolti: `console/static/app.html`, `console/static/app.js`, `console/static/app.css`.
- Avvio console: dalla cartella `console/`, `./console.exe` (o `go run .`) → `:8081`,
  reverse-proxy `/api` verso il core su `:8080`.
