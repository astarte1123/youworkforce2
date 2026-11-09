#!/usr/bin/env bash
# Launch a REAL Traccar server and wire its event forwarding to the YouWorkforce core.
# Evidence profile: UPSTREAM REALE (device -> Traccar -> forward -> core -> incident -> GoAlert).
#
# Prereqs: docker, the docker network `ywnet` (created by qa/run-goalert.sh), and the
# core running on :8080 reachable from the container as host.docker.internal:8080.
set -euo pipefail

IMAGE="${TRACCAR_IMAGE:-traccar/traccar:6.17.0}"   # pinned, verified in this repo
TOKEN="${INGEST_TOKEN:-acme-ingest-token}"
UI_PORT="${TRACCAR_UI_PORT:-8083}"
OSMAND_PORT="${TRACCAR_OSMAND_PORT:-5055}"
FWD="http://host.docker.internal:8080/api/ingress/traccar?token=${TOKEN}"
POS_FWD="http://host.docker.internal:8080/api/ingress/traccar/position?token=${TOKEN}"

docker network create ywnet >/dev/null 2>&1 || true
docker rm -f traccar >/dev/null 2>&1 || true

echo "starting $IMAGE ..."
docker run -d --name traccar --network ywnet \
  -p ${UI_PORT}:8082 -p ${OSMAND_PORT}:5055 -p ${OSMAND_PORT}:5055/udp \
  -e CONFIG_USE_ENVIRONMENT_VARIABLES=true \
  -e EVENT_FORWARD_URL="$FWD" \
  -e FORWARD_TYPE=json \
  -e FORWARD_URL="$POS_FWD" \
  --add-host host.docker.internal:host-gateway \
  "$IMAGE" >/dev/null

echo -n "waiting for Traccar UI on :${UI_PORT} "
for i in $(seq 1 30); do
  c=$(curl -s -o /dev/null -w "%{http_code}" -m 3 "http://localhost:${UI_PORT}/" 2>/dev/null || true)
  [ "$c" = "200" ] && { echo "up"; break; }
  echo -n "."; sleep 2
done
VER=$(curl -s -m 5 "http://localhost:${UI_PORT}/api/server" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
echo "Traccar version: ${VER:-unknown}"

# First user registered on a fresh install becomes administrator.
J=$(mktemp)
curl -s -X POST "http://localhost:${UI_PORT}/api/users" -H 'content-type: application/json' \
  -d '{"name":"Op","email":"op@acme.test","password":"Passw0rd!"}' >/dev/null 2>&1 || true
curl -s -c "$J" -X POST "http://localhost:${UI_PORT}/api/session" \
  -d "email=op@acme.test&password=Passw0rd!" >/dev/null
# Register the badge device (uniqueId must match core device external_id, source 'traccar').
curl -s -b "$J" -X POST "http://localhost:${UI_PORT}/api/devices" -H 'content-type: application/json' \
  -d '{"name":"Badge Mario","uniqueId":"BADGE-001"}' >/dev/null 2>&1 || true
echo "device BADGE-001 ready. Event forward -> ${FWD}"
echo "Send an SOS:  curl \"http://localhost:${OSMAND_PORT}/?id=BADGE-001&lat=45.47&lon=9.19&timestamp=\$(date +%s)&alarm=sos\""
