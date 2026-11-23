#!/usr/bin/env bash
# v3: generate data with the new fault types (incl. OPC UA), retrain Qwen3-1.7B, benchmark.
#   bash bench/foreman/retrain_v2.sh <cowpoke password>
set -u
cd "$(dirname "$0")"
PASS=${1:?cowpoke admin password}
PY=~/.venvs/mlx/bin
LOG=data/mlx/runs/status.log
say() { echo "$(date '+%H:%M') v3: $*" | tee -a "$LOG"; }

say "generating 4000 scenarios"
python3 gen.py --scenarios 4000 --batch 40 --seed 3 --settle 10 --out data/faults-s3.jsonl > data/gen-s3.log 2>&1 \
  || { say "generation FAILED: $(tail -1 data/gen-s3.log)"; exit 1; }
say "generated $(wc -l < data/faults-s3.jsonl) examples"

cat data/faults-s1.jsonl data/faults-s2.jsonl data/faults-s3.jsonl > data/faults-v3.jsonl
mkdir -p data/mlx-v3
python3 to_chat.py data/faults-v3.jsonl data/mlx-v3/train.jsonl data/mlx-v3/valid.jsonl --limit 4400 --valid-frac 0.045 --seed 3 \
  | tee -a "$LOG"

dir=data/mlx/runs/qwen3-1.7b-v3
mkdir -p $dir
cat > $dir/lora.yaml <<EOF
model: mlx-community/Qwen3-1.7B-4bit
train: true
data: data/mlx-v3
fine_tune_type: lora
mask_prompt: true
num_layers: 8
batch_size: 1
iters: 1000
learning_rate: 1.0e-4
steps_per_report: 50
steps_per_eval: 1000
val_batches: 8
save_every: 250
max_seq_length: 1024
grad_checkpoint: true
adapter_path: $dir/adapters
lora_parameters: {rank: 8, scale: 20.0, dropout: 0.0}
seed: 3
EOF
say "training qwen3-1.7b-v3"
$PY/mlx_lm.lora --config $dir/lora.yaml > $dir/train.log 2>&1 || { say "training FAILED: $(tail -1 $dir/train.log)"; exit 1; }
say "trained; val $(tr '\r' '\n' < $dir/train.log | sed 's/\x1b\[[0-9;]*m//g' | grep -E '^ *1000 +val' | awk '{print $3}')"

PORT=8088
$PY/mlx_lm.server --model mlx-community/Qwen3-1.7B-4bit --adapter-path $dir/adapters --port $PORT --temp 0 > $dir/server.log 2>&1 &
srv=$!
for _ in $(seq 120); do curl -s -m 2 "http://127.0.0.1:$PORT/v1/models" >/dev/null && break; sleep 2; done
for set in "scenarios.json|http://127.0.0.1:18090|trained" "scenarios_novel.json|http://127.0.0.1:18092|novel" "scenarios_unseen.json|http://127.0.0.1:18093|unseen"; do
  IFS='|' read -r file url tag <<< "$set"
  for r in --rules ""; do
    out=$(python3 bench.py --model x --openai "http://127.0.0.1:$PORT" --mode evidence $r --no-format --cowpoke "$url" \
      --scenarios "$file" --label "v3-$tag-qwen3-1.7b${r:+-rules}" --password "$PASS" 2>&1 | tail -1)
    say "$tag ${r:-no-rules}: $(python3 -c 'import json,sys
try:
  s=json.loads(sys.argv[1]); print("passed %d/12, service %d, cause %d, %.1f s" % (s["passed"], s["service_ok"], s["cause_ok"], s["avg_seconds"]))
except Exception: print("bench FAILED")' "$out")"
  done
done
kill $srv
say "DONE"
