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

Su entrambe le mappe (e sulle Piantine) resta attiva la **gestione completa delle aree
geofence** (disegno ⬡, rinomina, colore, modifica forma, associa device/eventi, elimina) —
riservata ai `tenant_admin`. Nella Mappa allarmi il campo **"Colore area Geofence"** è
allineato agli altri campi della barra (Dal / Al / Tipo).

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

## 6. Responsive (mobile / tablet)

- **Navigazione mobile**: sotto 820px la barra laterale diventa un **drawer a scomparsa**.
  In alto a sinistra compare il pulsante **☰** (hamburger) che apre/chiude il menu; uno
  sfondo scuro (scrim) chiude al tocco e il menu si chiude anche scegliendo una voce.
  (Prima la barra veniva **nascosta** su mobile, rendendo l'app non navigabile da telefono.)
- **Tabelle**: su schermi stretti (≤640px) le tabelle **scorrono in orizzontale** dentro la
  loro card invece di sforare la pagina.
- **Griglie e barre**: KPI e colonne si impilano progressivamente; le mappe passano a una
  sola colonna (mappa sopra, dettaglio sotto); le barre strumenti vanno a capo.

## 7. Lingua (i18n IT/EN)

La console ha un selettore **IT/EN** (in alto a destra; la scelta è salvata e ricaricata).
L'infrastruttura è `t('chiave','testo IT')`: con lingua EN usa la traduzione se presente,
altrimenti mostra il testo italiano (fallback sicuro). Sono tradotte le **schermate chiave**
(login, menu/gruppi, **Centrale allarmi** con tab/ricerca/paginazione/colonne, **Dispositivi**,
**Mappa allarmi/dispositivi**, **Piantine**); le altre viste restano in italiano finché non
vengono migrate. Per aggiungere una lingua/stringa: inserire la chiave in `I18N.en` (in
`app.html`) e avvolgere il testo con `t('chiave','testo IT')`.

## Note tecniche
- File coinvolti: `console/static/app.html`, `console/static/app.js`, `console/static/app.css`.
- Avvio console: dalla cartella `console/`, `./console.exe` (o `go run .`) → `:8081`,
  reverse-proxy `/api` verso il core su `:8080`.
