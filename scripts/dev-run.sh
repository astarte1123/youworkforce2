#!/usr/bin/env bash
# Dev launcher supervisionato per YouWorkforce (Windows/Git Bash o Linux).
# Avvia e MANTIENE SU core (Rust :8080) + console (Go :8081), riavviandoli se escono.
# Risolve: console.exe che esce quando il core sparisce durante un rebuild, e `go run`
# che su Windows muore con 0xc0000006 (STATUS_IN_PAGE_ERROR) -> usiamo il .exe buildato.
#
# Uso:   bash scripts/dev-run.sh
# Stop:  Ctrl-C (ferma entrambi).
# Env:   YW_WORKER_STACK_MB (default 32), YW_DB_HOST_PORT (default 55432).
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DBPORT="${YW_DB_HOST_PORT:-55432}"
export YW_DB_OWNER="postgres://postgres:postgres@localhost:${DBPORT}/youworkforce"
export YW_DB_RT="postgres://app_rt:app_rt_pw@localhost:${DBPORT}/youworkforce"
export TEST_CLOCK="${TEST_CLOCK:-1}"
export YW_SEED_DEMO="${YW_SEED_DEMO:-1}"
export YW_WORKER_STACK_MB="${YW_WORKER_STACK_MB:-32}"
# chiave di cifratura segreti a riposo (dev); abilita enc:v1: su ai_connections (chiave del provider AI)
export YW_SECRET_KEY="${YW_SECRET_KEY:-dev-secret-key-youworkforce}"

say(){ printf '\033[36m[dev-run]\033[0m %s\n' "$1"; }

# 1) Postgres raggiungibile?
if ! (exec 3<>"/dev/tcp/localhost/${DBPORT}") 2>/dev/null; then
  say "Postgres non risponde su :${DBPORT}. Avvia ywdb, es.:"
  say "  docker run -d --name ywdb -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=youworkforce -p ${DBPORT}:5432 postgres:18"
  exit 1
fi
exec 3>&- 2>/dev/null || true

# 2) build una volta (core + console.exe); il core migra+seed all'avvio.
say "build core (release)…"; ( cd "$ROOT/core" && cargo build --release ) || { say "build core FALLITA"; exit 1; }
say "build console.exe…";   ( cd "$ROOT/console" && go build -o console.exe . ) || { say "build console FALLITA"; exit 1; }

PIDS=()
cleanup(){ say "stop…"; for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done; taskkill //F //IM yw-core.exe 2>/dev/null; taskkill //F //IM console.exe 2>/dev/null; exit 0; }
trap cleanup INT TERM

# supervisore: riavvia il comando se esce (backoff 2s)
supervise(){ local name="$1"; shift
  while true; do
    say "avvio $name"
    ( "$@" ) ; local rc=$?
    say "$name uscito (rc=$rc) -> riavvio tra 2s"
    sleep 2
  done
}

supervise core    bash -c "cd '$ROOT/core' && exec ./target/release/yw-core.exe" &
PIDS+=($!)
supervise console bash -c "cd '$ROOT/console' && exec ./console.exe" &
PIDS+=($!)

say "core -> http://localhost:8080  console -> http://localhost:8081  (Ctrl-C per fermare)"
wait
