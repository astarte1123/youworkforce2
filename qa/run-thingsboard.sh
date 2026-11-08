#!/usr/bin/env bash
# Launch a REAL ThingsBoard CE and wire its Rule Engine to forward critical telemetry
# to the YouWorkforce core. Evidence profile: UPSTREAM REALE.
#
# Flow proven: device (HTTP telemetry) -> ThingsBoard Rule Engine
#   -> "REST API Call" node (added to the Root Rule Chain) -> core /api/ingress/thingsboard
#   -> normalized event (sensore_critico if threshold breach) -> incident -> GoAlert.
#
# Prereqs: docker, network ywnet, core on :8080 reachable as host.docker.internal:8080.
# Requires `python` on PATH for the rule-chain wiring (JSON editing via urllib).
set -uo pipefail

IMAGE="${TB_IMAGE:-thingsboard/tb-postgres:3.8.1}"   # pinned, verified in this repo
TB="${TB_URL:-http://localhost:8084}"                 # host -> container :9090
USER_="${TB_USER:-tenant@thingsboard.org}"
PASS_="${TB_PASS:-tenant}"
FWD="http://host.docker.internal:8080/api/ingress/thingsboard?token=acme-ingest-token&device=\${deviceName}"

docker network create ywnet >/dev/null 2>&1 || true
if ! docker ps --format '{{.Names}}' | grep -q '^tb$'; then
  docker rm -f tb >/dev/null 2>&1 || true
  docker volume create tb-data >/dev/null
  echo "starting $IMAGE (first boot provisions the DB, ~40-90s) ..."
  docker run -d --name tb --network ywnet \
    -p 8084:9090 -p 1883:1883 -p 7070:7070 \
    --add-host host.docker.internal:host-gateway \
    -v tb-data:/data "$IMAGE" >/dev/null
fi

echo -n "waiting for ThingsBoard REST "
for i in $(seq 1 60); do
  c=$(curl -s -o /dev/null -w "%{http_code}" -m 3 "$TB/api/auth/login" 2>/dev/null || true)
  [ "$c" != "000" ] && { echo "up"; break; }
  echo -n "."; sleep 5
done

TOK=$(curl -s -X POST "$TB/api/auth/login" -H 'content-type: application/json' \
  -d "{\"username\":\"$USER_\",\"password\":\"$PASS_\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
[ -n "$TOK" ] || { echo "login failed"; exit 1; }
echo "ThingsBoard version: $(curl -s "$TB/api/system/info" -H "X-Authorization: Bearer $TOK" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"

# Discover the root rule chain and wire a REST API Call node onto "Post telemetry" (idempotent).
TB_URL="$TB" TB_TOKEN="$TOK" TB_FWD="$FWD" python - <<'PY'
import os, json, urllib.request, urllib.error
TB=os.environ["TB_URL"]; TOKEN=os.environ["TB_TOKEN"]; FWD=os.environ["TB_FWD"]
def req(method, path, body=None):
    data=json.dumps(body).encode() if body is not None else None
    r=urllib.request.Request(TB+path, data=data, method=method)
    r.add_header("X-Authorization","Bearer "+TOKEN); r.add_header("Content-Type","application/json")
    with urllib.request.urlopen(r) as resp:
        raw=resp.read().decode(); return json.loads(raw) if raw else {}
chains=req("GET","/api/ruleChains?pageSize=100&page=0")["data"]
rc=next(c["id"]["id"] for c in chains if c.get("root"))
m=req("GET","/api/ruleChain/%s/metadata"%rc)
NAME="Forward to YouWorkforce"
if any(n.get("name")==NAME for n in m["nodes"]):
    print("rule chain already wired"); raise SystemExit
node={"type":"org.thingsboard.rule.engine.rest.TbRestApiCallNode","name":NAME,
      "debugMode":False,"singletonMode":False,"configurationVersion":0,
      "configuration":{"restEndpointUrlPattern":FWD,"requestMethod":"POST",
        "useSimpleClientHttpFactory":False,"parseToPlainText":False,"ignoreRequestBody":False,
        "enableProxy":False,"useSystemProxyProperties":False,"readTimeoutMs":0,
        "maxParallelRequestsCount":0,"headers":{"Content-Type":"application/json"},
        "credentials":{"type":"anonymous"},"maxInMemoryBufferSizeInKb":256},
      "additionalInfo":{"layoutX":1000,"layoutY":60}}
idx=len(m["nodes"]); m["nodes"].append(node)
sw=next(i for i,n in enumerate(m["nodes"]) if n["type"].endswith("TbMsgTypeSwitchNode"))
m["connections"].append({"fromIndex":sw,"toIndex":idx,"type":"Post telemetry"})
out=req("POST","/api/ruleChain/metadata",m)
print("rule chain wired:", "Forward to YouWorkforce" in [n["name"] for n in out["nodes"]])
PY

# Create a device on the DEFAULT profile (so telemetry flows through the ROOT chain we edited).
DID=$(curl -s -X POST "$TB/api/device" -H "X-Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"name":"SENSOR-DEF"}' | sed -n 's/.*"id":{"entityType":"DEVICE","id":"\([^"]*\)".*/\1/p')
if [ -z "$DID" ]; then
  DID=$(curl -s "$TB/api/tenant/devices?deviceName=SENSOR-DEF" -H "X-Authorization: Bearer $TOK" | sed -n 's/.*"id":{"entityType":"DEVICE","id":"\([^"]*\)".*/\1/p')
fi
AT=$(curl -s "$TB/api/device/$DID/credentials" -H "X-Authorization: Bearer $TOK" | sed -n 's/.*"credentialsId":"\([^"]*\)".*/\1/p')
echo "device SENSOR-DEF ready (default profile). access token: $AT"
echo "Send a critical reading: curl -X POST $TB/api/v1/$AT/telemetry -H 'content-type: application/json' -d '{\"temperature\":96}'"
echo "ThingsBoard UI: $TB  (login $USER_ / $PASS_)"
