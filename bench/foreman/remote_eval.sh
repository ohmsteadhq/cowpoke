#!/usr/bin/env bash
# Benchmark GGUF models on another machine with llama.cpp's server image, driving the
# benchmark from here. The container is temporary: started per model, removed after.
#
#   bash bench/foreman/remote_eval.sh <ssh host> <host LAN IP> <cowpoke password> <dir with ggufs on host>
set -u
cd "$(dirname "$0")"
HOST=${1:?ssh host}; IP=${2:?LAN IP}; PASS=${3:?cowpoke password}; DIR=${4:-foreman-test}
PORT=18089
LOG=data/mlx/runs/status.log
say() { echo "$(date '+%H:%M') $*" | tee -a "$LOG"; }

for name in qwen3-1.7b smollm3-3b qwen3-4b; do
  f=foreman-$name-q4_k_m.gguf
  extra=""
  case $name in qwen3*) extra='--reasoning-budget 0' ;; esac
  ssh "$HOST" "docker rm -f foreman-test >/dev/null 2>&1; docker run -d --name foreman-test -p $IP:$PORT:8080 \
    -v \$HOME/$DIR:/models:ro ghcr.io/ggml-org/llama.cpp:server -m /models/$f -c 4096 -t \$(nproc) --jinja $extra" >/dev/null
  for _ in $(seq 120); do curl -s -m 2 "http://$IP:$PORT/health" | grep -q ok && break; sleep 2; done
  for r in --rules ""; do
    out=$(python3 bench.py --model x --openai "http://$IP:$PORT" --mode evidence $r --no-format \
      --label "$HOST-$name-evidence${r:+-rules}" --password "$PASS" 2>&1 | tail -1)
    say "$HOST $name ${r:-no-rules}: $(python3 -c 'import json,sys
try:
  s=json.loads(sys.argv[1]); print("passed %d/12, service %d, cause %d, %.1f s" % (s["passed"], s["service_ok"], s["cause_ok"], s["avg_seconds"]))
except Exception: print("bench FAILED")' "$out")"
  done
  ssh "$HOST" "docker rm -f foreman-test >/dev/null 2>&1"
done
say "REMOTE EVAL DONE ($HOST)"
