#!/usr/bin/env bash
# Launch a REAL GoAlert (v0.35.0) with its own Postgres, migrate, seed 3 users, run.
# Evidence profile: UPSTREAM REALE. Test recipients only; delivery transport is the
# step to wire per-site (SMS/voice/webhook) — not configured here.
set -e
IMG="goalert/goalert:latest"
GURL="postgres://goalert:goalert@goalert-db:5432/goalert?sslmode=disable"
docker network create ywnet 2>/dev/null || true
docker rm -f goalert goalert-db 2>/dev/null || true

docker run -d --name goalert-db --network ywnet -e POSTGRES_USER=goalert -e POSTGRES_PASSWORD=goalert -e POSTGRES_DB=goalert postgres:18 >/dev/null
echo -n "waiting goalert-db"; for i in $(seq 1 30); do docker exec goalert-db pg_isready -U goalert >/dev/null 2>&1 && break; echo -n .; sleep 1; done; echo

docker run --rm --network ywnet "$IMG" goalert migrate --db-url "$GURL" >/dev/null
docker run --rm --network ywnet "$IMG" goalert add-user --db-url "$GURL" --user admin   --pass goalertadmin1 --admin >/dev/null
docker run --rm --network ywnet "$IMG" goalert add-user --db-url "$GURL" --user oncall1 --pass oncall1pass >/dev/null
docker run --rm --network ywnet "$IMG" goalert add-user --db-url "$GURL" --user oncall2 --pass oncall2pass >/dev/null

docker run -d --name goalert --network ywnet -p 8082:8081 "$IMG" \
  goalert --db-url "$GURL" --listen 0.0.0.0:8081 --public-url http://localhost:8082 --disable-https-redirect >/dev/null
echo -n "waiting goalert"; for i in $(seq 1 30); do curl -s -o /dev/null -w '%{http_code}' localhost:8082/health 2>/dev/null | grep -q 200 && break; echo -n .; sleep 1; done; echo
echo "GoAlert up on http://localhost:8082  (admin / goalertadmin1)"
echo "Next: bash qa/goalert-setup.sh   # creates EP(2 steps)+service+key, prints KEY"
