# Progetto: i18n / multilingua YouWorkforce

Stato: PROPOSTA (plan-only) · Owner: console (frontend) + core (messaggi API) · Priorità: media
(sblocca clienti esteri). Oggi: **tutto hardcoded in italiano** (`lang="it"`, stringhe inline nei
template, `toLocaleString('it-IT')`, messaggi d'errore API in italiano). Nessun meccanismo i18n.

## Obiettivo
Rendere l'interfaccia (e i messaggi utente dell'API) traducibile, con selettore lingua per utente,
partendo da IT + EN, senza riscrivere la logica. Non è una feature singola: è una **migrazione di
contenuti** su una superficie di stringhe ampia (console SPA ~migliaia di literal). Va fatta a fasi.

## Architettura proposta
**Frontend (console/static):**
- `i18n.js`: `t(key, params?)` con dizionari `it`/`en` (file JSON separati `i18n/it.json`,
  `i18n/en.json`). Fallback: chiave mancante → italiano → chiave grezza.
- Lingua: `?lang=` / `localStorage.yw_lang` / `navigator.language`; selettore nel footer della
  sidebar; `<html lang>` aggiornato dinamicamente.
- Date/numeri: sostituire `toLocaleString('it-IT')` con `toLocaleString(LANG)` (helper `fmtDate`).
- Migrazione stringhe a fasi (vedi roadmap): le viste restano funzionanti in IT finché non migrate.

**Backend (core):**
- Messaggi d'errore/`detail` utente passano per una tabella chiavi→traduzioni, scelta via header
  `Accept-Language` del client. Gli audit/log restano in una lingua tecnica (non tradotti).
- Le etichette di dominio (etype, stati) già hanno label lato UI (`ETYPE_LAB`): tradurle lì.

## Perché a fasi (non un big-bang)
Estrarre ~migliaia di literal inline da `app.html` in un colpo è rischioso (regressioni UI diffuse).
Meglio: infrastruttura + migrazione incrementale per vista, con IT come sorgente di verità.

## Roadmap
1. **Infra**: `i18n.js` + `t()` + dizionari it/en + selettore + persistenza + `fmtDate`. Nessuna
   stringa migrata ancora (IT invariato).
2. **Shell + login + nav** (incl. `NAV_GROUPS`): poche decine di stringhe → prima EN visibile.
3. **Viste core** una alla volta (Centrale, I miei allarmi, Dashboard, Mappa…): estrai in `*.json`.
4. **Messaggi API utente** (core): chiavi + `Accept-Language`.
5. **Etichette dominio** (etype/stati/ruoli) + date/numeri localizzati.
6. **QA**: smoke per lingua (EN attiva → nessuna chiave grezza nelle viste migrate).

## Rischi
- Regressioni UI durante l'estrazione → mitigare con migrazione per-vista + review.
- Chiavi mancanti → fallback IT visibile, mai pagina rotta.
- PWA worker ha stringhe proprie → stesso meccanismo, dizionario separato.

## Stima
Infra: piccola. Migrazione completa stringhe: grande (è il grosso). EN "shell+nav+login" rapido;
copertura totale a fasi su più interventi.

## Non in scope
RTL, pluralizzazione avanzata (ICU) — valutabili dopo EN base.
