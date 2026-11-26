"""Genera console/static/i18n-en.js dal dizionario console/tools/i18n/en.json.

Uso (dalla radice del repo):
    python -I -X utf8 console/tools/i18n/extract.py          # elenca i testi italiani delle pagine
    python -I -X utf8 console/tools/i18n/build.py            # rigenera i18n-en.js
    python -I -X utf8 console/tools/i18n/build.py --missing  # testi senza traduzione (da aggiungere a en.json)

en.json: {"phrases": {"testo italiano": "English"}, "templates": {"Testo con {0}": "Text with {0}"}}
"""
import json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..'))
SRC = os.path.join(HERE, 'en.json')
OUT = os.path.join(ROOT, 'console', 'static', 'i18n-en.js')
STRINGS = os.path.join(HERE, 'strings.json')


def main():
    d = json.load(open(SRC, encoding='utf-8'))
    ph, tpl = d.get('phrases', {}), d.get('templates', {})
    bad = [k for k, v in tpl.items() if sorted(re.findall(r'\{\d+\}', k)) != sorted(re.findall(r'\{\d+\}', v))]
    if bad:
        sys.exit('segnaposto diversi tra italiano e inglese: %r' % bad[:5])
    if '--missing' in sys.argv:
        s = json.load(open(STRINGS, encoding='utf-8'))
        miss = [k for k in s['phrases'] if k not in ph] + [k for k in s['templates'] if k not in tpl]
        print('\n'.join(miss)); print('-- %d senza traduzione' % len(miss), file=sys.stderr); return
    # una parola o etichetta in minuscolo compare spesso con l'iniziale maiuscola (stati, pulsanti)
    for k, v in list(ph.items()):
        if k[:1].islower() and v[:1].islower():
            ph.setdefault(k[:1].upper() + k[1:], v[:1].upper() + v[1:])
    # i modelli più lunghi (più specifici) prima
    tl = sorted(([k, v] for k, v in tpl.items() if k != v), key=lambda kv: -len(kv[0]))
    body = {'phrases': {k: v for k, v in sorted(ph.items()) if k != v}, 'templates': tl}
    js = ('/* GENERATO da console/tools/i18n/build.py — non modificare a mano: modifica en.json. */\n'
          'window.YW_I18N_EN=' + json.dumps(body, ensure_ascii=False, separators=(',', ':')) + ';\n')
    open(OUT, 'w', encoding='utf-8', newline='\n').write(js)
    print('%s: %d frasi, %d modelli, %d KB' % (os.path.relpath(OUT, ROOT), len(body['phrases']), len(tl), len(js.encode()) // 1024))


if __name__ == '__main__':
    main()
