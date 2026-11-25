#!/usr/bin/env bash
# AR-1: ogni rotta del core ha autenticazione, controllo del modulo e permesso (o un'eccezione
# motivata in qa/handler_guards_allow.txt). Statico: non serve il core.
cd "$(dirname "$0")/.." && python -I qa/handler_guards.py
