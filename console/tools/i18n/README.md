# Traduzione inglese dell'interfaccia

Le pagine sono scritte in italiano. Con la lingua **EN** (selettore IT/EN in testata e nella pagina
di accesso, salvato in `localStorage.yw_lang`; senza scelta vale la lingua del browser) il motore
`console/static/i18n.js` traduce ogni testo mostrato cercandolo nel dizionario
`console/static/i18n-en.js`. Vale per console, superadmin, app del lavoratore e pagina di invito.

Cosa traduce:

- il testo tra i tag, `placeholder`, `title`, `aria-label`, i pulsanti `input`, il titolo della pagina;
- anche quello che compare dopo (render, pannelli, toast, finestre di conferma), con un `MutationObserver`;
- le frasi esatte; i **modelli** con parti variabili (`Commessa {0}`, `Passo {0} di {1}`); le forme
  composte «12 allarmi», «— Testo», «Sezione · Voce», «Etichetta: valore»;
- i messaggi del core (errori, passi della configurazione guidata, nomi dei moduli, tipi di evento
  predefiniti), perché sono nel dizionario anche loro.

Cosa non traduce: i dati (nomi di sedi, persone, percorsi, valori), le note scritte dagli utenti e
i nomi dei tipi di evento cambiati dal superadmin nel catalogo. Un elemento con `data-noi18n` non
viene toccato.

## Aggiungere o cambiare testi

Dalla radice del repo:

```sh
python -I -X utf8 console/tools/i18n/extract.py            # rilegge pagine e core -> strings.json
python -I -X utf8 console/tools/i18n/build.py --missing    # elenca i testi senza traduzione
# aggiungere le traduzioni in en.json ("phrases" o "templates")
python -I -X utf8 console/tools/i18n/build.py              # rigenera console/static/i18n-en.js
```

`en.json` è la fonte (si committa); `i18n-en.js` è generato (si committa anche lui: la console lo
serve così com'è). Nei modelli i segnaposto `{0}`, `{1}` devono essere gli stessi nelle due lingue
(`build.py` si ferma se non lo sono). Le traduzioni seguono il glossario (`docs/glossario.md`):
Gruppo di escalation = *Escalation group*, Percorso di ronda = *Patrol route*, ecc.

Dopo un cambio del dizionario l'app del lavoratore ha bisogno di una nuova versione della cache
(`yw-shell-vN` in `console/static/worker/sw.js`).
