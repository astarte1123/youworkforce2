#!/usr/bin/env bash
# AR-7: glossario unico (docs/glossario.md): nei testi della console e nel manuale utente non
# ricompaiono i nomi vecchi (giro, gruppo allarme, on-call, override, ladder). Statico.
cd "$(dirname "$0")/.." && python -I qa/glossary-check.py
