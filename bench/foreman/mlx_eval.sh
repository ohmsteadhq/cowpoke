#!/usr/bin/env bash
# Benchmark fine-tuned adapters served by MLX (mlx_lm.server), which applies each model's
# own chat template, the same one used in training. Waits for finetune_all.sh to finish.
#
#   bash bench/foreman/mlx_eval.sh <cowpoke password>
set -u
cd "$(dirname "$0")"
PASS=${1:?cowpoke admin password}
PY=~/.venvs/mlx/bin
RUNS=data/mlx/runs
LOG=$RUNS/status.log
PORT=8088
say() { echo "$(date '+%H:%M') $*" | tee -a "$LOG"; }

while pgrep -f "finetune_all.sh" >/dev/null; do sleep 30; done
while pgrep -f "Python.*bench.py --model" >/dev/null; do sleep 15; done
ollama stop cowpoke-foreman:qwen3-1.7b >/dev/null 2>&1

for entry in \
  "qwen3-4b|mlx-community/Qwen3-4B-4bit|data/mlx/adapters" \
  "gemma4-e2b|mlx-community/gemma-4-e2b-it-4bit|$RUNS/gemma4-e2b/adapters" \
  "smollm3-3b|mlx-community/SmolLM3-3B-4bit|$RUNS/smollm3-3b/adapters" \
  "llama3.2-3b|mlx-community/Llama-3.2-3B-Instruct-4bit|$RUNS/llama3.2-3b/adapters" \
  "qwen3-1.7b|mlx-community/Qwen3-1.7B-4bit|$RUNS/qwen3-1.7b/adapters"; do
  IFS='|' read -r name hf adapters <<< "$entry"
  [ -f "$adapters/adapters.safetensors" ] || { say "mlx $name: no adapter, skipped"; continue; }
  $PY/mlx_lm.server --model "$hf" --adapter-path "$adapters" --port $PORT --temp 0 > "$RUNS/server-$name.log" 2>&1 &
  srv=$!
  for _ in $(seq 120); do curl -s -m 2 "http://127.0.0.1:$PORT/v1/models" >/dev/null && break; sleep 2; done
  for r in --rules ""; do
    python3 bench.py --model "$hf" --openai "http://127.0.0.1:$PORT" --mode evidence $r --no-format \
      --label "mlx-$name-evidence${r:+-rules}" --password "$PASS" > "$RUNS/bench-$name${r:+-rules}.log" 2>&1
    sum=$(tail -1 "$RUNS/bench-$name${r:+-rules}.log")
    say "mlx $name ${r:-no-rules}: $(python3 -c 'import json,sys
try:
  s=json.loads(sys.argv[1]); print("passed %d/12, service %d, cause %d, %.1f s" % (s["passed"], s["service_ok"], s["cause_ok"], s["avg_seconds"]))
except Exception: print("bench FAILED")' "$sum")"
  done
  kill $srv; wait $srv 2>/dev/null
done
say "MLX EVAL DONE"
