#!/usr/bin/env bash
# AR-2: query sul pool privilegiato (senza RLS) che toccano tabelle di tenant senza tenant_id:
# vanno filtrate o motivate in qa/owner_pool_allow.txt. Statico: non serve il core.
cd "$(dirname "$0")/.." && python -I qa/owner_pool_audit.py
